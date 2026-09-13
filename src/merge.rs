//! `[[merge]]` — semantic multi-file config merging.
//!
//! Unlike `[[link]]` which shares an inode via hardlink/junction/symlink,
//! `[[merge]]` keeps source and target as independent files and synchronizes
//! their content semantically using format-aware deep merging.
//!
//! - `apply`: injects base configuration from source into target without
//!   destroying target-only local runtime state (e.g., project histories,
//!   machine-specific paths).
//! - `absorb`: extracts clean configuration updates and schema additions
//!   from target back into source, filtering out specified `ignore_keys`.

use std::fs;

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use teravars::deep_merge;
use toml::{Table, Value};
use tracing::{debug, info, warn};

use crate::Result;
use crate::paths;
use crate::template::{self, Engine};

/// One `[[merge]]` declaration in `config.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MergeEntry {
    /// Relative path to the source base template in `$DOTFILES`.
    pub src: Utf8PathBuf,
    /// Destination live target path. Tera-rendered, `~` expanded.
    pub dst: String,
    /// Format of the configuration file. Default: auto-detected from extension (or TOML).
    #[serde(default)]
    pub format: Option<MergeFormat>,
    /// Keys to ignore when absorbing from target into source.
    /// Supports dot-notation for nested tables (e.g. `"marketplaces.openai-bundled"`).
    #[serde(default)]
    pub ignore_keys: Vec<String>,
    /// Optional Tera boolean predicate gating this entry.
    #[serde(default)]
    pub when: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MergeFormat {
    #[default]
    Toml,
    Json,
}

impl MergeEntry {
    /// Evaluate the `when` clause against the given Tera context.
    pub fn is_active(&self, engine: &mut Engine, ctx: &teravars::Context) -> Result<bool> {
        let Some(when) = &self.when else {
            return Ok(true);
        };
        template::eval_truthy(when, engine, ctx)
    }

    /// Resolve the destination path (Tera-render + `~` expansion).
    pub fn resolve_dst(&self, engine: &mut Engine, ctx: &teravars::Context) -> Result<Utf8PathBuf> {
        let rendered = engine.render(&self.dst, ctx)?;
        let expanded = paths::expand_tilde(&rendered);
        Ok(expanded)
    }
}

/// Filter a TOML table in-place, removing any paths listed in `ignore_keys`.
/// Supports top-level keys (`"projects"`, `"notify"`) and dot-separated
/// nested paths (`"marketplaces.openai-bundled"`). Empty parent tables left
/// behind after nested removal are pruned.
pub fn filter_table(table: &mut Table, ignore_keys: &[String]) {
    for pattern in ignore_keys {
        let parts: Vec<&str> = pattern.split('.').collect();
        remove_nested(table, &parts);
    }
}

fn remove_nested(table: &mut Table, parts: &[&str]) -> bool {
    if parts.is_empty() {
        return false;
    }
    if parts.len() == 1 {
        let key = parts[0];
        if key == "*" {
            table.clear();
            return true;
        }
        table.remove(key);
        return table.is_empty();
    }

    let key = parts[0];
    let rest = &parts[1..];

    let mut should_remove_key = false;
    if key == "*" {
        // Apply rest to all child tables
        let mut keys_to_remove = Vec::new();
        for (k, v) in table.iter_mut() {
            if let Value::Table(sub_table) = v {
                if remove_nested(sub_table, rest) {
                    keys_to_remove.push(k.clone());
                }
            }
        }
        for k in keys_to_remove {
            table.remove(&k);
        }
        return table.is_empty();
    }

    if let Some(Value::Table(sub_table)) = table.get_mut(key) {
        if remove_nested(sub_table, rest) {
            should_remove_key = true;
        }
    }

    if should_remove_key {
        table.remove(key);
    }

    table.is_empty()
}

/// Merge `base` into `target` in-place.
/// Keys present in `base` are injected/overwritten in `target`.
/// Keys exclusive to `target` (local state) are preserved intact.
pub fn merge_toml(base: &Table, target: &mut Table) {
    deep_merge(target, base.clone());
}

/// Absorb changes from `target` into `base`.
/// `ignore_keys` are stripped from `target` before merging into `base`.
/// Returns `true` if `base` was modified.
pub fn absorb_toml(base: &mut Table, target: &Table, ignore_keys: &[String]) -> bool {
    let mut clean_target = target.clone();
    filter_table(&mut clean_target, ignore_keys);

    let old_base = base.clone();
    deep_merge(base, clean_target);
    *base != old_base
}

/// Check if `target` has changes that should be absorbed into `base`
/// (ignoring `ignore_keys`).
pub fn check_drift(base: &Table, target: &Table, ignore_keys: &[String]) -> bool {
    let mut clean_target = target.clone();
    filter_table(&mut clean_target, ignore_keys);

    let mut test_base = base.clone();
    deep_merge(&mut test_base, clean_target);
    test_base != *base
}

/// Execute apply for a single merge entry.
pub fn apply_entry(
    entry: &MergeEntry,
    source_root: &Utf8Path,
    engine: &mut Engine,
    ctx: &teravars::Context,
    dry_run: bool,
) -> Result<()> {
    let src_path = source_root.join(&entry.src);
    if !src_path.exists() {
        warn!("merge source does not exist: {src_path}");
        return Ok(());
    }

    let dst_path = entry.resolve_dst(engine, ctx)?;

    let src_content = fs::read_to_string(&src_path)
        .with_context(|| format!("reading merge source {src_path}"))?;
    let src_table: Table = toml::from_str(&src_content)
        .with_context(|| format!("parsing TOML in merge source {src_path}"))?;

    if !dst_path.exists() {
        info!("merge: creating new target {dst_path} from {src_path}");
        if !dry_run {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating directory {parent}"))?;
            }
            fs::write(&dst_path, src_content)
                .with_context(|| format!("writing merge target {dst_path}"))?;
        }
        return Ok(());
    }

    let dst_content = fs::read_to_string(&dst_path)
        .with_context(|| format!("reading merge target {dst_path}"))?;
    let mut dst_table: Table = match toml::from_str(&dst_content) {
        Ok(t) => t,
        Err(e) => {
            warn!("failed to parse merge target {dst_path} as TOML: {e}; skipping");
            return Ok(());
        }
    };

    let original_dst = dst_table.clone();
    merge_toml(&src_table, &mut dst_table);

    if dst_table != original_dst {
        info!("merge: updating target {dst_path} with base {src_path}");
        if !dry_run {
            let rendered = toml::to_string_pretty(&dst_table)
                .with_context(|| format!("serializing merged TOML for {dst_path}"))?;
            fs::write(&dst_path, rendered)
                .with_context(|| format!("writing merged target {dst_path}"))?;
        }
    } else {
        debug!("merge: target {dst_path} already up-to-date with {src_path}");
    }

    Ok(())
}

/// Execute absorb for a single merge entry.
pub fn absorb_entry(
    entry: &MergeEntry,
    source_root: &Utf8Path,
    engine: &mut Engine,
    ctx: &teravars::Context,
    dry_run: bool,
) -> Result<bool> {
    let src_path = source_root.join(&entry.src);
    let dst_path = entry.resolve_dst(engine, ctx)?;

    if !dst_path.exists() {
        debug!("merge absorb: target does not exist: {dst_path}");
        return Ok(false);
    }

    let src_content = if src_path.exists() {
        fs::read_to_string(&src_path).with_context(|| format!("reading merge source {src_path}"))?
    } else {
        String::new()
    };
    let mut src_table: Table = if src_content.trim().is_empty() {
        Table::new()
    } else {
        toml::from_str(&src_content)
            .with_context(|| format!("parsing TOML in merge source {src_path}"))?
    };

    let dst_content = fs::read_to_string(&dst_path)
        .with_context(|| format!("reading merge target {dst_path}"))?;
    let dst_table: Table = match toml::from_str(&dst_content) {
        Ok(t) => t,
        Err(e) => {
            warn!("failed to parse merge target {dst_path} as TOML: {e}; skipping absorb");
            return Ok(false);
        }
    };

    let changed = absorb_toml(&mut src_table, &dst_table, &entry.ignore_keys);
    if changed {
        info!("merge absorb: updating source {src_path} from target {dst_path}");
        if !dry_run {
            if let Some(parent) = src_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating directory {parent}"))?;
            }
            let rendered = toml::to_string_pretty(&src_table)
                .with_context(|| format!("serializing absorbed TOML for {src_path}"))?;
            fs::write(&src_path, rendered)
                .with_context(|| format!("writing absorbed source {src_path}"))?;
        }
    }

    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_table_top_level_keys() {
        let mut table: Table = r#"
model = "gpt-6"
notify = "something"
[projects]
"c:\\foo" = "trusted"
"#
        .parse()
        .unwrap();

        let ignore = vec!["notify".to_string(), "projects".to_string()];
        filter_table(&mut table, &ignore);

        assert_eq!(table.len(), 1);
        assert_eq!(table.get("model").unwrap().as_str().unwrap(), "gpt-6");
        assert!(!table.contains_key("notify"));
        assert!(!table.contains_key("projects"));
    }

    #[test]
    fn test_filter_table_nested_dot_keys() {
        let mut table: Table = r#"
[marketplaces.openai-bundled]
source = "local"
[marketplaces.claude]
source = "git"
"#
        .parse()
        .unwrap();

        let ignore = vec!["marketplaces.openai-bundled".to_string()];
        filter_table(&mut table, &ignore);

        let marketplaces = table.get("marketplaces").unwrap().as_table().unwrap();
        assert_eq!(marketplaces.len(), 1);
        assert!(marketplaces.contains_key("claude"));
        assert!(!marketplaces.contains_key("openai-bundled"));
    }

    #[test]
    fn test_filter_table_wildcard_nested() {
        let mut table: Table = r#"
model = "gpt-6"
[projects.proj1]
trust = "trusted"
[projects.proj2]
trust = "untrusted"
"#
        .parse()
        .unwrap();

        let ignore = vec!["projects.*".to_string()];
        filter_table(&mut table, &ignore);

        assert!(!table.contains_key("projects"));
        assert_eq!(table.get("model").unwrap().as_str().unwrap(), "gpt-6");
    }

    #[test]
    fn test_filter_table_cleans_up_empty_parents() {
        let mut table: Table = r#"
model = "test"
[tui.model_availability_nux]
gpt-6 = 4
"#
        .parse()
        .unwrap();

        let ignore = vec!["tui.model_availability_nux".to_string()];
        filter_table(&mut table, &ignore);

        assert!(
            !table.contains_key("tui"),
            "empty parent table should be pruned"
        );
        assert_eq!(table.get("model").unwrap().as_str().unwrap(), "test");
    }

    #[test]
    fn test_apply_merge_preserves_target_unique() {
        let base: Table = r#"
model = "gpt-6-astra"
model_reasoning_effort = "high"
[features]
js_repl = false
"#
        .parse()
        .unwrap();

        let mut target: Table = r#"
model = "gpt-5"
notify = ["path/to/exe"]
[features]
js_repl = true
[projects]
"c:\\test" = "trusted"
"#
        .parse()
        .unwrap();

        merge_toml(&base, &mut target);

        // Overwritten by base
        assert_eq!(
            target.get("model").unwrap().as_str().unwrap(),
            "gpt-6-astra"
        );
        assert_eq!(
            target
                .get("model_reasoning_effort")
                .unwrap()
                .as_str()
                .unwrap(),
            "high"
        );
        let features = target.get("features").unwrap().as_table().unwrap();
        assert!(!features.get("js_repl").unwrap().as_bool().unwrap());

        // Preserved from target
        assert!(target.contains_key("projects"));
        assert!(target.contains_key("notify"));
    }

    #[test]
    fn test_absorb_merges_new_live_settings_excluding_ignored() {
        let mut base: Table = r#"
model = "gpt-6-astra"
"#
        .parse()
        .unwrap();

        let target: Table = r#"
model = "gpt-6-astra"
new_feature_setting = true
[desktop]
ambient = true
notify = ["local/path"]
[projects]
"c:\\secret" = "trusted"
"#
        .parse()
        .unwrap();

        let ignore = vec!["notify".to_string(), "projects".to_string()];
        let changed = absorb_toml(&mut base, &target, &ignore);

        assert!(changed);
        assert!(base.get("new_feature_setting").unwrap().as_bool().unwrap());
        let desktop = base.get("desktop").unwrap().as_table().unwrap();
        assert!(desktop.get("ambient").unwrap().as_bool().unwrap());

        // Ignored keys must not be present in base
        assert!(!base.contains_key("notify"));
        assert!(!base.contains_key("projects"));
    }

    #[test]
    fn test_check_drift_ignores_blacklisted_keys() {
        let base: Table = r#"
model = "gpt-6-astra"
"#
        .parse()
        .unwrap();

        // Target has new projects and notify, but identical user settings
        let target_only_ignored: Table = r#"
model = "gpt-6-astra"
notify = ["local/path"]
[projects]
"c:\\secret" = "trusted"
"#
        .parse()
        .unwrap();

        let ignore = vec!["notify".to_string(), "projects".to_string()];
        assert!(!check_drift(&base, &target_only_ignored, &ignore));

        // Target changed user setting
        let target_with_user_change: Table = r#"
model = "gpt-6-omni"
notify = ["local/path"]
"#
        .parse()
        .unwrap();
        assert!(check_drift(&base, &target_with_user_change, &ignore));
    }
}
