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
/// file's extension (`.json` → JSON, `.yml` / `.yaml` → YAML, anything else
/// → TOML). Both sides are loaded into the same `toml::Table` model so
/// `ignore_keys`, deep merge and drift detection behave identically across
/// formats.
///
/// JSON and YAML limits: `null` and integers outside `i64` have no TOML
/// equivalent and fail to parse (a target is then skipped with a warning),
/// and output keys are sorted. YAML additionally rejects non-string mapping
/// keys, application tags, binary values and non-finite floats. A rewrite
/// loses comments, anchors and quoting style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Json,
    Yaml,
}

impl Format {
    pub fn of(path: &Utf8Path) -> Self {
        match path.extension() {
            Some(ext) if ext.eq_ignore_ascii_case("json") => Self::Json,
            Some(ext) if ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml") => {
                Self::Yaml
            }
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
            Self::Yaml => {
                validate_yaml_scalars(text)?;
                // A blank or comment-only YAML file is the null document.
                match serde_saphyr::from_str_with_options(text, yaml_options())? {
                    None => Ok(Table::new()),
                    Some(YamlValue(Value::Table(table))) => Ok(table),
                    Some(_) => anyhow::bail!("YAML merge document must be a mapping"),
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
            // Keep every value on one line: the default folds strings over
            // 80 characters (long paths) across several.
            Self::Yaml => Ok(serde_saphyr::to_string_with_options(
                table,
                serde_saphyr::ser_options! { prefer_block_scalars: false },
            )?),
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

/// YAML 1.1 readers take plain `yes` / `no` / `on` / `off` for booleans and
/// `0755` / `1_000` for numbers; YAML 1.2 readers take them for strings. A
/// merge rewrites files it does not own, so read them as the text they are
/// (the writer quotes such strings, which every reader then agrees on)
/// rather than change their spelling.
fn yaml_options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        strict_booleans: true,
        leading_zero_numbers_as_strings: true,
        underscored_numbers_as_strings: true,
        reject_unsupported_tags: true,
    }
}

// Check resolved tags and integer spellings before `deserialize_any` can
// discard application tags or coerce unsupported values.
fn validate_yaml_scalars(text: &str) -> anyhow::Result<()> {
    use serde_saphyr::granit_parser::{Parser, ScalarStyle};

    for event in Parser::new_from_str(text) {
        let (event, span) = event.map_err(|error| anyhow::anyhow!("{error}"))?;
        if let Some(tag) = event.tag() {
            // In particular, serde-saphyr accepts local !map as an alias for !!map.
            if tag.is_yaml_core_schema() || tag.parts() == ("", "!") {
                continue;
            }
            let location = span.tag_start.unwrap_or(span.start);
            let is_binary = tag
                .suffix_in_namespace("tag:yaml.org,2002:")
                .or_else(|| tag.suffix_in_namespace("!"))
                .is_some_and(|name| matches!(name.as_ref(), "binary" | "!binary"));
            if is_binary {
                anyhow::bail!(
                    "YAML binary has no TOML equivalent at line {}, column {}",
                    location.line(),
                    location.col() + 1
                );
            }
            anyhow::bail!(
                "unsupported YAML tag {} at line {}, column {}",
                tag.original(),
                location.line(),
                location.col() + 1
            );
        }
        if let Some((value, ScalarStyle::Plain)) = event.scalar()
            && yaml_integer_out_of_range(value)
        {
            anyhow::bail!(
                "YAML integer {value} is outside the i64 range at line {}, column {}",
                span.start.line(),
                span.start.col() + 1
            );
        }
    }
    Ok(())
}

fn yaml_integer_out_of_range(value: &str) -> bool {
    let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
    let (radix, digits) = if let Some(digits) = digits.strip_prefix("0x") {
        (16, digits)
    } else if let Some(digits) = digits.strip_prefix("0o") {
        (8, digits)
    } else if let Some(digits) = digits.strip_prefix("0b") {
        (2, digits)
    } else if digits.len() > 1 && digits.starts_with('0') {
        return false; // Leading-zero decimals are strings.
    } else {
        (10, digits)
    };
    let max = i64::MAX as u64 + u64::from(value.starts_with('-'));
    let too_large = match u64::from_str_radix(digits, radix) {
        Ok(magnitude) => magnitude > max,
        Err(error) => *error.kind() == std::num::IntErrorKind::PosOverflow,
    };
    // Overflow may be reported before a trailing character (e.g. `.0`) is read.
    too_large && digits.bytes().all(|byte| char::from(byte).is_digit(radix))
}

/// A YAML node converted to its TOML equivalent while parsing, so a value
/// without one fails at its line and column instead of being coerced.
struct YamlValue(Value);

impl<'de> Deserialize<'de> for YamlValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, SeqAccess};

        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = YamlValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a YAML value with a TOML equivalent")
            }

            fn visit_bool<E>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(YamlValue(Value::Boolean(v)))
            }

            fn visit_i64<E>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(YamlValue(Value::Integer(v)))
            }

            fn visit_u64<E: Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                i64::try_from(v)
                    .map(|v| YamlValue(Value::Integer(v)))
                    .map_err(|_| E::custom(format!("integer {v} is outside the i64 range")))
            }

            fn visit_f64<E>(self, v: f64) -> std::result::Result<Self::Value, E> {
                Ok(YamlValue(Value::Float(v)))
            }

            fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(YamlValue(Value::String(v.to_owned())))
            }

            fn visit_string<E>(self, v: String) -> std::result::Result<Self::Value, E> {
                Ok(YamlValue(Value::String(v)))
            }

            fn visit_unit<E: Error>(self) -> std::result::Result<Self::Value, E> {
                Err(E::custom("null has no TOML equivalent"))
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(YamlValue(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(YamlValue(Value::Array(values)))
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut table = Table::new();
                while let Some(YamlKey(key)) = map.next_key()? {
                    let YamlValue(value) = map.next_value()?;
                    table.insert(key, value);
                }
                Ok(YamlValue(Value::Table(table)))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// A mapping key the document spells as a string. Asking for a `String`
/// would also accept `1:` or `true:` and write them back as quoted text,
/// changing the key's type for the application.
struct YamlKey(String);

impl<'de> Deserialize<'de> for YamlKey {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = YamlKey;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string key")
            }

            fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(YamlKey(v.to_owned()))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
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
            if let Value::Table(sub_table) = v
                && remove_nested(sub_table, rest)
            {
                keys_to_remove.push(k.clone());
            }
        }
        for k in keys_to_remove {
            table.remove(&k);
        }
        return table.is_empty();
    }

    if let Some(Value::Table(sub_table)) = table.get_mut(key)
        && remove_nested(sub_table, rest)
    {
        should_remove_key = true;
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

    #[test]
    fn test_yaml_format_absorb_drops_ignored_nested_key_and_roundtrips() {
        assert_eq!(Format::of(Utf8Path::new("a/config.yml")), Format::Yaml);
        assert_eq!(
            Format::of(Utf8Path::new("a/config.base.YAML")),
            Format::Yaml
        );

        let mut base = Format::Yaml.parse("theme:\n  dark: nord\n").unwrap();
        let target = Format::Yaml
            .parse(
                "theme:\n  dark: dracula\nskills:\n  enabled: true\n  customDirectories:\n    - C:/Users/me/skills\n",
            )
            .unwrap();
        let ignore = vec!["skills.customDirectories".to_string()];

        assert!(absorb_toml(&mut base, &target, &ignore));
        let rendered = Format::Yaml.render(&base).unwrap();
        assert_eq!(
            rendered,
            "skills:\n  enabled: true\ntheme:\n  dark: dracula\n"
        );

        // apply direction keeps the target-only ignored key intact
        let mut live = target.clone();
        merge_toml(&base, &mut live);
        assert_eq!(live, target);
        assert!(!check_drift(&base, &live, &ignore));
    }

    #[test]
    fn test_yaml_render_keeps_long_and_multiline_strings_on_one_line() {
        let long = "C:/Users/me/a directory with spaces/".repeat(4);
        let mut table = Table::new();
        table.insert("note".into(), Value::String("two\nlines".into()));
        table.insert("paths".into(), Value::Array(vec![Value::String(long)]));

        let rendered = Format::Yaml.render(&table).unwrap();
        assert_eq!(rendered.lines().count(), 3, "{rendered}");
        assert_eq!(Format::Yaml.parse(&rendered).unwrap(), table);
    }

    /// A rewritten file must hand the application the same types back,
    /// whichever YAML version its reader follows: strings and keys that look
    /// like booleans, nulls or numbers are written quoted.
    #[test]
    fn test_yaml_render_keeps_scalar_lookalikes_as_strings() {
        let lookalikes = [
            "no",
            "Yes",
            "on",
            "OFF",
            "y",
            "N",
            "true",
            "False",
            "null",
            "~",
            "123",
            "-7",
            "1.5",
            "1e3",
            "0x1f",
            "0755",
            "1_000",
            "12:30",
            ".nan",
            "",
            " padded ",
            "a: b",
            "a #b",
            "- item",
            "# c",
            "two\nlines",
            "quote\"d",
            "it's",
            "*alias",
            "&anchor",
            "!tag",
            "[x",
            "{x",
            "C:\\Users\\me",
            "<<",
        ];
        let mut table = Table::new();
        for (i, text) in lookalikes.iter().enumerate() {
            table.insert(format!("k{i:02}"), Value::String(text.to_string()));
            table.insert(text.to_string(), Value::Integer(i as i64));
        }

        let rendered = Format::Yaml.render(&table).unwrap();
        assert_eq!(Format::Yaml.parse(&rendered).unwrap(), table, "{rendered}");
        // A YAML 1.1 reader takes a bare `no` for false and `on:` for a
        // boolean key.
        assert!(rendered.contains("k00: \"no\"\n"), "{rendered}");
        assert!(rendered.contains("\"on\": 2\n"), "{rendered}");
    }

    /// Plain scalars a YAML 1.1 reader would convert keep their spelling, so
    /// a merge never turns `no` into `false` or `0755` into `755`.
    #[test]
    fn test_yaml_parse_reads_ambiguous_plain_scalars_as_text() {
        let table = Format::Yaml
            .parse("on: push\nanswer: no\nmode: 0755\ncount: 1_000\nflag: true\nport: 8080\n")
            .unwrap();
        assert_eq!(table["on"].as_str(), Some("push"));
        assert_eq!(table["answer"].as_str(), Some("no"));
        assert_eq!(table["mode"].as_str(), Some("0755"));
        assert_eq!(table["count"].as_str(), Some("1_000"));
        assert_eq!(table["flag"].as_bool(), Some(true));
        assert_eq!(table["port"].as_integer(), Some(8080));
    }

    #[test]
    fn test_yaml_parse_limits() {
        // Blank and comment-only files are an empty mapping, like blank TOML.
        assert!(Format::Yaml.parse("").unwrap().is_empty());
        assert!(Format::Yaml.parse("# nothing yet\n").unwrap().is_empty());

        for unsupported in [
            "setting: null\n",
            "setting: ~\n",
            "setting:\n",
            "list:\n  - 1\n  -\n",
            "1: numeric key\n",
            "true: boolean key\n",
            "setting: !custom value\n",
            "setting: .nan\n",
            "setting: .inf\n",
            "setting: 18446744073709551615\n",
            "twice: 1\ntwice: 2\n",
            "first: 1\n---\nsecond: 2\n",
            "- top\n- level\n- list\n",
            "just a string\n",
        ] {
            assert!(
                Format::Yaml.parse(unsupported).is_err(),
                "accepted {unsupported:?}"
            );
        }

        for (unsupported, reason, location) in [
            (
                "settings: [-9223372036854775809]\n",
                "outside the i64 range",
                "line 1, column 12",
            ),
            (
                "setting: 18446744073709551616\n",
                "outside the i64 range",
                "line 1, column 10",
            ),
            (
                "setting: 0x10000000000000000\n",
                "outside the i64 range",
                "line 1, column 10",
            ),
            (
                "setting: !!binary SGVsbG8=\n",
                "binary has no TOML equivalent",
                "line 1, column 10",
            ),
            (
                "setting: !<tag:yaml.org,2002:binary> SGVsbG8=\n",
                "binary has no TOML equivalent",
                "line 1, column 10",
            ),
            (
                "%TAG !b! tag:yaml.org,2002:bi\n---\nsetting: !b!nary SGVsbG8=\n",
                "binary has no TOML equivalent",
                "line 3, column 10",
            ),
            (
                "setting: !map {enabled: true}\n",
                "unsupported YAML tag !map",
                "line 1, column 10",
            ),
            (
                "setting: [!str true]\n",
                "unsupported YAML tag !str",
                "line 1, column 11",
            ),
            (
                "%TAG !! tag:example.org,2026:\n---\nsetting: !!map {enabled: true}\n",
                "unsupported YAML tag",
                "line 3, column 10",
            ),
        ] {
            let error = Format::Yaml.parse(unsupported).unwrap_err().to_string();
            assert!(
                error.contains(reason) && error.contains(location),
                "{error}"
            );
        }

        let table = Format::Yaml
            .parse("big: 9223372036854775807\nsmall: -9223372036854775808\nratio: 0.5\n")
            .unwrap();
        assert_eq!(table["big"].as_integer(), Some(i64::MAX));
        assert_eq!(table["small"].as_integer(), Some(i64::MIN));
        assert_eq!(table["ratio"].as_float(), Some(0.5));

        let table = Format::Yaml
            .parse(concat!(
                "hex: -0x8000000000000000\n",
                "float: 18446744073709551616.0\n",
                "quoted: '-9223372036854775809'\n",
                "text: !!str 18446744073709551616\n",
                "padded: 018446744073709551616\n",
                "underscored: 18_446_744_073_709_551_616\n",
                "binary_text: '!!binary SGVsbG8='\n",
                "non_specific: ! true\n",
            ))
            .unwrap();
        assert_eq!(table["hex"].as_integer(), Some(i64::MIN));
        assert_eq!(
            table["float"].as_float(),
            Some(18_446_744_073_709_551_616.0)
        );
        assert_eq!(table["quoted"].as_str(), Some("-9223372036854775809"));
        assert_eq!(table["text"].as_str(), Some("18446744073709551616"));
        assert_eq!(table["padded"].as_str(), Some("018446744073709551616"));
        assert_eq!(
            table["underscored"].as_str(),
            Some("18_446_744_073_709_551_616")
        );
        assert_eq!(table["binary_text"].as_str(), Some("!!binary SGVsbG8="));
        assert_eq!(table["non_specific"].as_str(), Some("true"));

        let table = Format::Yaml
            .parse("%TAG !core! tag:yaml.org,2002:\n---\nsetting: !core!map {enabled: true}\n")
            .unwrap();
        assert_eq!(table["setting"]["enabled"].as_bool(), Some(true));
    }
}
