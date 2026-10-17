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
use toml::{Table, Value};
use tracing::{debug, info, warn};

use crate::Result;
use crate::backup;
use crate::paths;
use crate::template::{self, Engine};

/// On-disk syntax of a `[[merge]]` source/target, picked from the source
/// file's extension (`.json` → JSON, anything else → TOML). Both sides are
/// loaded into the same `toml::Table` model so `ignore_keys`, deep merge and
/// drift detection behave identically across formats.
///
/// JSON limits: `null` and integers outside `i64` have no TOML equivalent
/// and fail to parse (a target is then skipped with a warning), and output
/// keys are sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Json,
}

impl Format {
    pub fn of(path: &Utf8Path) -> Self {
        match path.extension() {
            Some(ext) if ext.eq_ignore_ascii_case("json") => Self::Json,
            _ => Self::Toml,
        }
    }

    pub fn parse(self, text: &str) -> anyhow::Result<Table> {
        match self {
            Self::Toml => Ok(toml::from_str(text)?),
            // An empty TOML file parses to an empty table; an empty JSON
            // file is not valid JSON, so mirror the TOML behavior instead of
            // leaving a blank target unrecoverable.
            Self::Json if text.trim().is_empty() => Ok(Table::new()),
            Self::Json => {
                // Keep integer tokens intact until their range is checked;
                // without arbitrary_precision, serde_json rounds oversized
                // integers to f64 before we can reject them.
                let value: serde_json::Value = serde_json::from_str(text)?;
                match json_to_toml(value)? {
                    Value::Table(table) => Ok(table),
                    _ => anyhow::bail!("JSON merge document must be an object"),
                }
            }
        }
    }

    pub fn render(self, table: &Table) -> anyhow::Result<String> {
        match self {
            Self::Toml => Ok(toml::to_string_pretty(table)?),
            Self::Json => {
                let mut s = serde_json::to_string_pretty(table)?;
                s.push('\n');
                Ok(s)
            }
        }
    }
}

// Convert explicitly: arbitrary-precision numbers may deserialize through
// serde as private maps, which TOML would otherwise accept as real tables.
fn json_to_toml(value: serde_json::Value) -> anyhow::Result<Value> {
    Ok(match value {
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Value::Integer(integer)
            } else if number.is_f64() {
                Value::Float(number.as_f64().context("converting JSON float")?)
            } else {
                anyhow::bail!("JSON number {number} cannot be represented as a TOML number");
            }
        }
        serde_json::Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(json_to_toml)
                .collect::<anyhow::Result<_>>()?,
        ),
        serde_json::Value::Object(values) => Value::Table(
            values
                .into_iter()
                .map(|(key, value)| Ok((key, json_to_toml(value)?)))
                .collect::<anyhow::Result<_>>()?,
        ),
        serde_json::Value::String(value) => Value::String(value),
        serde_json::Value::Bool(value) => Value::Boolean(value),
        serde_json::Value::Null => anyhow::bail!("JSON null has no TOML equivalent"),
    })
}

/// One `[[merge]]` declaration in `config.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MergeEntry {
    /// Relative path to the source base template in `$DOTFILES`.
    pub src: Utf8PathBuf,
    /// Destination live target path. Tera-rendered, `~` expanded.
    pub dst: String,
    /// Keys treated as target's exclusive local state: excluded from
    /// `absorb` (never pulled into source) and from `apply` (base never
    /// overwrites them in target either), and from drift comparisons.
    /// Supports dot-notation for nested tables (e.g. `"marketplaces.openai-bundled"`).
    #[serde(default)]
    pub ignore_keys: Vec<String>,
    /// Optional Tera boolean predicate gating this entry.
    #[serde(default)]
    pub when: Option<String>,
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

/// Deep-merge `overlay` into `base`: nested tables recurse, everything
/// else (including arrays) is replaced wholesale.
///
/// This is deliberately *not* `teravars::deep_merge`, which appends
/// overlay arrays onto base arrays — the right call for layering
/// `config.toml` + `config.local.toml` exactly once at load time. A
/// `[[merge]]` entry instead runs `apply`/`absorb` on every invocation,
/// so array-append would duplicate the same elements on every run
/// instead of converging to a stable, idempotent result.
fn deep_merge(base: &mut Table, overlay: Table) {
    for (k, v) in overlay {
        match (base.remove(&k), v) {
            (Some(Value::Table(mut b)), Value::Table(o)) => {
                deep_merge(&mut b, o);
                base.insert(k, Value::Table(b));
            }
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
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

/// Check if `base` and `target` (outside `ignore_keys`) have diverged in
/// either direction: `target` holds changes `absorb` would pull into
/// `base`, or `base` holds settings `apply` hasn't pushed to `target`
/// yet (e.g. a key newly added to `base`, which `target` simply lacks).
pub fn check_drift(base: &Table, target: &Table, ignore_keys: &[String]) -> bool {
    let mut clean_base = base.clone();
    filter_table(&mut clean_base, ignore_keys);
    let mut clean_target = target.clone();
    filter_table(&mut clean_target, ignore_keys);

    // apply direction: would base → target change target?
    let mut applied = clean_target.clone();
    deep_merge(&mut applied, clean_base.clone());
    if applied != clean_target {
        return true;
    }

    // absorb direction: would target → base change base?
    let mut test_base = clean_base.clone();
    deep_merge(&mut test_base, clean_target);
    test_base != clean_base
}

/// [`check_drift`] over raw file text. A side that fails to parse (e.g. a
/// JSON target holding `null`) counts as drift: `apply` skips such a target,
/// so reporting it in sync would hide that it never gets merged.
pub fn check_drift_text(
    format: Format,
    src_text: &str,
    dst_text: &str,
    ignore_keys: &[String],
) -> bool {
    match (format.parse(src_text), format.parse(dst_text)) {
        (Ok(src), Ok(dst)) => check_drift(&src, &dst, ignore_keys),
        _ => true,
    }
}

/// Both sides rendered without `ignore_keys`, for a filtered diff. If either
/// side fails to parse the raw texts are returned unfiltered, so the diff
/// shows the real file content instead of a parse failure standing in as an
/// empty document.
pub fn filtered_for_diff(
    format: Format,
    src_text: String,
    dst_text: String,
    ignore_keys: &[String],
) -> (String, String) {
    let (Ok(mut src), Ok(mut dst)) = (format.parse(&src_text), format.parse(&dst_text)) else {
        return (src_text, dst_text);
    };
    filter_table(&mut src, ignore_keys);
    filter_table(&mut dst, ignore_keys);
    (
        format.render(&src).unwrap_or(src_text),
        format.render(&dst).unwrap_or(dst_text),
    )
}

/// Back up `target`'s current content into `.yui/backup/...` before a
/// `[[merge]]` write overwrites it. `apply_entry`/`absorb_entry` each
/// overwrite one side (target or source) of a merge based on an mtime
/// comparison; a coarse filesystem mtime, clock skew, or checked-out
/// files sharing a timestamp can misjudge that comparison, so the
/// write must stay recoverable the same way every other yui mutation
/// is (`backup_existing` in `cmd::apply`).
fn backup_before_overwrite(target: &Utf8Path, backup_root: &Utf8Path) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
    let abs_target = crate::cmd::absolutize(target)?;
    let ts = backup::current_timestamp("%Y%m%d_%H%M%S%3f")?;
    let bp = paths::append_timestamp(&paths::mirror_into_backup(backup_root, &abs_target), &ts);
    info!("backup → {bp}");
    backup::backup_file(target, &bp)?;
    Ok(())
}

/// Execute apply for a single merge entry.
pub fn apply_entry(
    entry: &MergeEntry,
    source_root: &Utf8Path,
    engine: &mut Engine,
    ctx: &teravars::Context,
    dry_run: bool,
    backup_root: &Utf8Path,
) -> Result<()> {
    let src_path = source_root.join(&entry.src);
    if !src_path.exists() {
        warn!("merge source does not exist: {src_path}");
        return Ok(());
    }

    let dst_path = entry.resolve_dst(engine, ctx)?;

    let src_content = fs::read_to_string(&src_path)
        .with_context(|| format!("reading merge source {src_path}"))?;
    let format = Format::of(&src_path);
    let mut src_table: Table = format
        .parse(&src_content)
        .with_context(|| format!("parsing merge source {src_path}"))?;

    if !dst_path.exists() {
        info!("merge: creating new target {dst_path} from {src_path}");
        if !dry_run {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating directory {parent}"))?;
            }
            // A blank source is a valid empty table, but blank text is not
            // a valid *JSON* document: render it instead of copying it.
            let new_content = if src_content.trim().is_empty() {
                format.render(&src_table)?
            } else {
                src_content
            };
            fs::write(&dst_path, new_content)
                .with_context(|| format!("writing merge target {dst_path}"))?;
        }
        return Ok(());
    }

    let dst_content = fs::read_to_string(&dst_path)
        .with_context(|| format!("reading merge target {dst_path}"))?;
    let mut dst_table: Table = match format.parse(&dst_content) {
        Ok(t) => t,
        Err(e) => {
            warn!("failed to parse merge target {dst_path}: {e}; skipping");
            return Ok(());
        }
    };

    // `ignore_keys` marks a key as target's exclusive local state (that's
    // what makes it safe for `check_drift` to exclude from both sides when
    // deciding whether an anomaly prompt is even needed). Base must not
    // clobber that state here just because it happens to also carry a
    // value under the same key — otherwise a target-only edit to an
    // ignored key gets silently overwritten the moment `check_drift`
    // (correctly) reports no drift and this runs unprompted.
    filter_table(&mut src_table, &entry.ignore_keys);

    let original_dst = dst_table.clone();
    merge_toml(&src_table, &mut dst_table);

    if dst_table != original_dst {
        info!("merge: updating target {dst_path} with base {src_path}");
        if !dry_run {
            backup_before_overwrite(&dst_path, backup_root)?;
            let rendered = format
                .render(&dst_table)
                .with_context(|| format!("serializing merged {dst_path}"))?;
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
    backup_root: &Utf8Path,
) -> Result<bool> {
    let src_path = source_root.join(&entry.src);
    let dst_path = entry.resolve_dst(engine, ctx)?;

    if !dst_path.exists() {
        debug!("merge absorb: target does not exist: {dst_path}");
        return Ok(false);
    }

    let format = Format::of(&src_path);

    let src_content = if src_path.exists() {
        fs::read_to_string(&src_path).with_context(|| format!("reading merge source {src_path}"))?
    } else {
        String::new()
    };
    let mut src_table: Table = if src_content.trim().is_empty() {
        Table::new()
    } else {
        format
            .parse(&src_content)
            .with_context(|| format!("parsing merge source {src_path}"))?
    };

    let dst_content = fs::read_to_string(&dst_path)
        .with_context(|| format!("reading merge target {dst_path}"))?;
    let dst_table: Table = match format.parse(&dst_content) {
        Ok(t) => t,
        Err(e) => {
            warn!("failed to parse merge target {dst_path}: {e}; skipping absorb");
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
            backup_before_overwrite(&src_path, backup_root)?;
            let rendered = Format::of(&src_path)
                .render(&src_table)
                .with_context(|| format!("serializing absorbed {src_path}"))?;
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

    #[test]
    fn test_check_drift_detects_key_new_to_base() {
        // `apply` hasn't run yet: target predates the setting base just
        // gained. This must count as drift even though `target` has
        // nothing extra for `absorb` to pull back.
        let base: Table = r#"
model = "gpt-6-astra"
new_setting = true
"#
        .parse()
        .unwrap();

        let target: Table = r#"
model = "gpt-6-astra"
"#
        .parse()
        .unwrap();

        assert!(check_drift(&base, &target, &[]));
    }

    #[test]
    fn test_check_drift_ignores_blacklisted_key_also_present_in_base() {
        // `base` (not just `target`) carries a value under an ignored key.
        // A target-only edit to that same ignored key must not register as
        // drift — `ignore_keys` means "outside comparison" on both sides,
        // not just the target side.
        let base: Table = r#"
model = "gpt-6-astra"
notify = "base"
"#
        .parse()
        .unwrap();

        let target_only_ignored_change: Table = r#"
model = "gpt-6-astra"
notify = "local"
"#
        .parse()
        .unwrap();

        let ignore = vec!["notify".to_string()];
        assert!(!check_drift(&base, &target_only_ignored_change, &ignore));
    }

    #[test]
    fn test_merge_toml_overwrites_arrays_instead_of_appending() {
        // `merge_toml` runs on every `apply`. If arrays appended (as
        // `teravars::deep_merge` does for layering config files), a
        // second no-op apply would duplicate every element again.
        let base: Table = r#"
notify = ["path/to/exe"]
"#
        .parse()
        .unwrap();

        let mut target: Table = r#"
notify = ["path/to/exe"]
"#
        .parse()
        .unwrap();

        merge_toml(&base, &mut target);
        merge_toml(&base, &mut target);

        let notify = target.get("notify").unwrap().as_array().unwrap();
        assert_eq!(notify.len(), 1, "array should be replaced, not appended");
    }

    #[test]
    fn test_json_rejects_out_of_range_integers() {
        for number in [
            "-9223372036854775809",
            "9223372036854775808",
            "18446744073709551616",
            "999999999999999999999999999999999999999999",
        ] {
            for text in [
                format!(r#"{{"id":{number}}}"#),
                format!(r#"{{"nested":{{"ids":[{number}]}}}}"#),
            ] {
                assert!(Format::Json.parse(&text).is_err(), "accepted {text}");
            }
        }
    }

    #[test]
    fn test_json_preserves_noncanonical_floats() {
        for (token, expected) in [
            ("1.50", 1.5),
            ("2.50", 2.5),
            ("0.10", 0.1),
            ("1E5", 1e5),
            ("1e+5", 1e5),
            ("1.0", 1.0),
        ] {
            let text = format!(r#"{{"scale":{token},"nested":{{"values":[{token}]}}}}"#);
            let table = Format::Json.parse(&text).unwrap();
            assert_eq!(table["scale"].as_float(), Some(expected), "{token}");
            assert_eq!(table["nested"]["values"][0].as_float(), Some(expected));
            let rendered = Format::Json.render(&table).unwrap();
            let json: serde_json::Value = serde_json::from_str(&rendered).unwrap();
            assert_eq!(json["scale"].as_f64(), Some(expected));
            let roundtripped = Format::Json.parse(&rendered).unwrap();
            assert!(!check_drift(&table, &roundtripped, &[]));
        }
    }

    #[test]
    fn test_json_apply_preserves_float_target_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let dst = root.join("target.json");
        fs::write(&dst, r#"{"scale":1.50,"theme":"light"}"#).unwrap();
        fs::write(root.join("base.json"), r#"{"theme":"dark"}"#).unwrap();
        let entry = MergeEntry {
            src: "base.json".into(),
            dst: dst.to_string(),
            ignore_keys: Vec::new(),
            when: None,
        };
        apply_entry(
            &entry,
            root,
            &mut Engine::new(),
            &teravars::Context::new(),
            false,
            &root.join("backup"),
        )
        .unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dst).unwrap()).unwrap();
        assert_eq!(json["scale"].as_f64(), Some(1.5));
        assert_eq!(json["theme"].as_str(), Some("dark"));
    }

    #[test]
    fn test_json_preserves_integer_limits_and_accepts_floats() {
        let text = r#"{"min":-9223372036854775808,"max":9223372036854775807,
            "fraction":1.5,"exponent":1e20,"label":"18446744073709551616"}"#;
        let table = Format::Json.parse(text).unwrap();
        assert_eq!(table["min"].as_integer(), Some(i64::MIN));
        assert_eq!(table["max"].as_integer(), Some(i64::MAX));
        assert_eq!(table["fraction"].as_float(), Some(1.5));
        assert_eq!(table["exponent"].as_float(), Some(1e20));
        assert_eq!(
            table,
            Format::Json
                .parse(&Format::Json.render(&table).unwrap())
                .unwrap()
        );
    }

    #[test]
    fn test_json_apply_skips_target_with_out_of_range_integer() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let src = root.join("base.json");
        let dst = root.join("target.json");
        let backup_root = root.join("backup");
        let original = r#"{"id":-9223372036854775809,"theme":"light"}"#;
        fs::write(&dst, original).unwrap();
        fs::write(&src, r#"{"theme":"dark"}"#).unwrap();
        let entry = MergeEntry {
            src: "base.json".into(),
            dst: dst.to_string(),
            ignore_keys: Vec::new(),
            when: None,
        };
        apply_entry(
            &entry,
            root,
            &mut Engine::new(),
            &teravars::Context::new(),
            false,
            &backup_root,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(dst).unwrap(), original);
        assert!(!backup_root.exists());
    }

    #[test]
    fn test_json_format_absorb_drops_ignored_keys_and_roundtrips() {
        assert_eq!(Format::of(Utf8Path::new("a/settings.json")), Format::Json);
        assert_eq!(Format::of(Utf8Path::new("a/config.toml")), Format::Toml);

        let mut base = Format::Json.parse(r#"{"colorScheme": "dark"}"#).unwrap();
        let target = Format::Json
            .parse(
                r#"{"colorScheme": "light", "notifications": true,
                    "trustedWorkspaces": ["/Users/me"]}"#,
            )
            .unwrap();
        let ignore = vec!["trustedWorkspaces".to_string()];

        assert!(absorb_toml(&mut base, &target, &ignore));
        let rendered = Format::Json.render(&base).unwrap();
        assert_eq!(
            rendered,
            "{\n  \"colorScheme\": \"light\",\n  \"notifications\": true\n}\n"
        );

        // apply direction keeps the target-only ignored key intact
        let mut live = target.clone();
        merge_toml(&base, &mut live);
        assert!(live.contains_key("trustedWorkspaces"));
        assert!(!check_drift(&base, &live, &ignore));
    }

    #[test]
    fn test_check_drift_text_reports_unparseable_side_as_drift() {
        // `null` is rejected by the JSON path, so `apply` skips this target;
        // status must not call it in sync.
        assert!(check_drift_text(
            Format::Json,
            "{}",
            r#"{"setting": null}"#,
            &[]
        ));
        assert!(!check_drift_text(Format::Json, "{}", "{}", &[]));
    }

    #[test]
    fn test_filtered_for_diff_falls_back_to_raw_text_on_parse_failure() {
        let bad = r#"{"setting": null}"#.to_string();
        let (src, dst) = filtered_for_diff(Format::Json, "{}".into(), bad.clone(), &[]);
        assert_eq!((src.as_str(), dst.as_str()), ("{}", bad.as_str()));
    }
}
