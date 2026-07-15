//! TOML schema for yui configuration.
//!
//! Loading flow:
//!   1. list `config.toml` + `config.*.toml` (alphabetical) + `config.local.toml` (last)
//!   2. for each file: Tera-render with `yui.*` + `env(…)` + accumulated `vars.*`
//!      from prior files → parse TOML → merge into accumulator (deep merge,
//!      arrays append).
//!   3. deserialize the final merged table into `Config`.
//!
//! Note: a file cannot reference its own `[vars]` keys from non-`[vars]`
//! sections (the file is rendered before its own vars are accumulated).
//! Use prior files in merge order if you need cross-section references.

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

use crate::vars::YuiVars;
use crate::{Error, Result, template};

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub vars: toml::Table,

    #[serde(default)]
    pub link: LinkConfig,

    #[serde(default)]
    pub mount: MountConfig,

    #[serde(default)]
    pub absorb: AbsorbConfig,

    #[serde(default)]
    pub render: RenderConfig,

    #[serde(default)]
    pub backup: BackupConfig,

    #[serde(default)]
    pub ui: UiConfig,

    #[serde(default)]
    pub hook: Vec<HookConfig>,

    #[serde(default)]
    pub secrets: SecretsConfig,
}

/// One hook = one script invocation triggered around `yui apply`.
///
/// The script lives at `$DOTFILES/<script>` (kept yui-agnostic — runnable
/// directly with no yui involvement); `command` + `args` decide how to
/// invoke it. Both are Tera-rendered with the standard yui context plus
/// `script_path` / `script_dir` / `script_name` / `script_stem` /
/// `script_ext`.
#[derive(Debug, Clone, Deserialize)]
pub struct HookConfig {
    /// Unique identifier — used as the state-tracking key and the
    /// argument to `yui hooks run <name>`.
    pub name: String,
    /// Script path relative to `$DOTFILES`. Hashed for `onchange` runs;
    /// also exposed to `command` / `args` Tera as `script_path` etc.
    pub script: Utf8PathBuf,

    /// Interpreter / command to invoke. Tera-rendered. Default `"bash"`.
    #[serde(default = "default_hook_command")]
    pub command: String,
    /// Arguments to `command`. Each element Tera-rendered. Default
    /// `["{{ script_path }}"]`.
    #[serde(default = "default_hook_args")]
    pub args: Vec<String>,

    /// Re-run policy. Default `Onchange`.
    #[serde(default)]
    pub when_run: WhenRun,
    /// Apply phase to fire on. Default `Post`.
    #[serde(default)]
    pub phase: HookPhase,

    /// Optional Tera bool predicate; absent = always eligible.
    #[serde(default)]
    pub when: Option<String>,
}

fn default_hook_command() -> String {
    "bash".to_string()
}

fn default_hook_args() -> Vec<String> {
    vec!["{{ script_path }}".to_string()]
}

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WhenRun {
    /// Run exactly once across the lifetime of the source repo. Tracked
    /// via `last_run_at` in `.yui/state.json`.
    Once,
    /// Run when the script content (SHA-256 of `script`) differs from
    /// the last successful run. Default — best fit for "re-run when I
    /// edit the bootstrap".
    #[default]
    Onchange,
    /// Run on every apply.
    Every,
}

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HookPhase {
    /// Before any render / link work — useful for prerequisite installs.
    Pre,
    /// After all linking finishes. Default — "I just `apply`'d, now
    /// reload the launchd / brew bundle / etc.".
    #[default]
    Post,
}

/// What yui does in the background when a newer release exists.
///
/// Background work runs once per `update_check_interval` (default
/// 24h), only on a real installed binary (dev builds are skipped by
/// kaishin), and is fully silent on any network / lock failure.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AutoUpdateMode {
    /// Do nothing — no check, no banner, no install.
    Off,
    /// Hit GitHub and show a banner at exit if a newer release is
    /// available, but never install. The user runs `yui self-update`
    /// themselves. (This was the 0.4.x default behavior.)
    Notify,
    /// Silently download + swap yui's own binary in the background;
    /// the running process keeps the old binary and the new version
    /// applies on next launch. Prints exactly one line on a real
    /// install. This is the default (opt-out).
    #[default]
    Install,
}

#[derive(Debug, Deserialize, Default)]
pub struct UiConfig {
    #[serde(default)]
    pub icons: IconsMode,
    /// Background auto-update behavior: `"off"` / `"notify"` /
    /// `"install"`. Default `install` (silent background install,
    /// applied on next launch). Powered by `kaishin::Checker`;
    /// mirrors renri / rvpm. The `YUI_NO_AUTOUPDATE` env var is a
    /// kill-switch that overrides this regardless of mode.
    ///
    /// `None` means the key was omitted from `config.toml` — that is
    /// kept distinct from an explicit `auto_update = "install"` so the
    /// deprecated `auto_update_check` alias only kicks in when the user
    /// gave no opinion here. [`UiConfig::update_mode`] resolves `None`
    /// to the default ([`AutoUpdateMode::Install`]).
    #[serde(default)]
    pub auto_update: Option<AutoUpdateMode>,
    /// DEPRECATED alias for `auto_update`, kept for backward
    /// compatibility. `Some(true)` → `notify`, `Some(false)` → `off`,
    /// `None` → no opinion (fall through to `auto_update` / default).
    /// `auto_update` always wins when both are set. Resolved by
    /// [`UiConfig::update_mode`], which emits a one-time migration
    /// warning when this key is present.
    #[serde(default)]
    pub auto_update_check: Option<bool>,
    /// Cadence override for the background update work (e.g.
    /// `"24h"`, `"1d"`, `"30m"`). Parsed by `kaishin::parse_interval`.
    /// `None` falls back to the kaishin default (24h).
    #[serde(default)]
    pub update_check_interval: Option<String>,
}

impl UiConfig {
    /// Resolve the effective auto-update mode, honoring the deprecated
    /// `auto_update_check` boolean alias.
    ///
    /// Precedence:
    /// 1. an explicit `auto_update` (any value the user actually wrote)
    ///    wins;
    /// 2. else the deprecated `auto_update_check` maps `true → notify`,
    ///    `false → off` (and emits a one-time migration warning);
    /// 3. else the default (`install`).
    ///
    /// Because `auto_update` is `Option`, an explicit
    /// `auto_update = "install"` is distinguishable from the omitted
    /// default and unambiguously wins over a stale `auto_update_check`.
    ///
    /// This is a **pure** resolver with no side effects — it is safe to
    /// call any number of times. The one-shot deprecation warning for
    /// the legacy `auto_update_check` alias is emitted separately at
    /// config load time (see [`load`] /
    /// [`UiConfig::warn_deprecated_auto_update_check`]), so the resolver
    /// stays a clean getter and the warning never duplicates or pollutes
    /// test output.
    pub fn update_mode(&self) -> AutoUpdateMode {
        // An explicit `auto_update` always wins — `Option` lets us tell
        // "user wrote auto_update = install" apart from the omitted
        // default, so the deprecated bool only matters when the user
        // gave no opinion here.
        if let Some(mode) = self.auto_update {
            return mode;
        }
        match self.auto_update_check {
            Some(true) => AutoUpdateMode::Notify,
            Some(false) => AutoUpdateMode::Off,
            None => AutoUpdateMode::default(),
        }
    }

    /// Emit the one-line deprecation warning for the legacy
    /// `auto_update_check` alias, but only when it is the key actually
    /// driving the effective mode (i.e. `auto_update` is unset but
    /// `auto_update_check` is present).
    ///
    /// Called exactly once from [`load`] at config-load time, so the
    /// warning fires at most once per process and [`update_mode`] can
    /// stay a pure getter.
    ///
    /// [`update_mode`]: Self::update_mode
    pub fn warn_deprecated_auto_update_check(&self) {
        if self.auto_update.is_some() {
            return;
        }
        if let Some(legacy) = self.auto_update_check {
            tracing::warn!(
                "[ui] auto_update_check is deprecated; use \
                 `auto_update = \"notify\"|\"off\"|\"install\"` instead \
                 ({} maps to {})",
                legacy,
                if legacy { "notify" } else { "off" },
            );
        }
    }
}

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum IconsMode {
    /// `✓ ✗ → ─` — works on any terminal that renders basic Unicode (default).
    #[default]
    Unicode,
    /// Nerd Font glyphs (`  →`) — requires a Nerd-Font-patched terminal font.
    Nerd,
    /// `[+] [-] -> -` — pure ASCII, for CI logs / SSH-into-legacy-tty.
    Ascii,
}

#[derive(Debug, Deserialize, Default)]
pub struct LinkConfig {
    #[serde(default)]
    pub file_mode: FileLinkMode,
    #[serde(default)]
    pub dir_mode: DirLinkMode,
}

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileLinkMode {
    #[default]
    Auto,
    Symlink,
    Hardlink,
}

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DirLinkMode {
    #[default]
    Auto,
    Symlink,
    Junction,
}

#[derive(Debug, Deserialize)]
pub struct MountConfig {
    #[serde(default)]
    pub default_strategy: MountStrategy,
    #[serde(default = "default_marker_filename")]
    pub marker_filename: String,
    #[serde(default)]
    pub entry: Vec<MountEntry>,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            default_strategy: MountStrategy::default(),
            marker_filename: default_marker_filename(),
            entry: Vec::new(),
        }
    }
}

fn default_marker_filename() -> String {
    ".yuilink".to_string()
}

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MountStrategy {
    #[default]
    Marker,
    PerFile,
}

#[derive(Debug, Deserialize)]
pub struct MountEntry {
    pub src: Utf8PathBuf,
    pub dst: String,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub strategy: Option<MountStrategy>,
}

#[derive(Debug, Deserialize)]
pub struct AbsorbConfig {
    #[serde(default = "default_true")]
    pub auto: bool,
    #[serde(default = "default_true")]
    pub require_clean_git: bool,
    #[serde(default)]
    pub on_anomaly: AnomalyAction,
}

impl Default for AbsorbConfig {
    fn default() -> Self {
        Self {
            auto: true,
            require_clean_git: true,
            on_anomaly: AnomalyAction::default(),
        }
    }
}

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AnomalyAction {
    #[default]
    Ask,
    Skip,
    Force,
}

#[derive(Debug, Deserialize)]
pub struct RenderConfig {
    #[serde(default = "default_true")]
    pub manage_gitignore: bool,
    #[serde(default)]
    pub rule: Vec<RenderRule>,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            manage_gitignore: true,
            rule: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RenderRule {
    pub r#match: String,
    #[serde(default)]
    pub when: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BackupConfig {
    #[serde(default = "default_backup_dir")]
    pub dir: String,
    #[serde(default = "default_ts_format")]
    pub timestamp_format: String,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            dir: default_backup_dir(),
            timestamp_format: default_ts_format(),
        }
    }
}

fn default_backup_dir() -> String {
    ".yui/backup".to_string()
}

/// `[secrets]` — wires the age encryption pipeline into apply.
///
/// `identity` is the path to your local age secret key file (NOT
/// committed). `recipients` is the public-key list every new
/// encryption is wrapped to — at minimum, the public key matching
/// `identity`, and any additional machines / users that should
/// also be able to decrypt. yui defaults the identity path to
/// `~/.config/yui/age.txt` and treats an empty `recipients` list
/// as "secrets feature off".
#[derive(Debug, Clone, Deserialize)]
pub struct SecretsConfig {
    /// Path to the X25519 secret key used by `apply` to decrypt
    /// `*.age` files. Plain (`AGE-SECRET-KEY-1…`) text, gitignored.
    /// Default `~/.config/yui/age.txt`.
    #[serde(default = "default_identity_path")]
    pub identity: String,

    /// Public keys that `*.age` files are encrypted to. X25519
    /// (`age1…`) is the everyday case and is what `yui secret init`
    /// adds. Plugin recipients (`age1<plugin>1…`) are also accepted
    /// — yui doesn't ship first-class commands for them, but if you
    /// hand-write a YubiKey / FIDO2 / TPM / Secure Enclave / 1P
    /// recipient here it'll be honored, and the matching
    /// `age-plugin-*` binary on `$PATH` lets `age` itself decrypt
    /// those stanzas. (yui's apply uses the X25519 in `identity`
    /// only, so plugin recipients add a *parallel* decrypt path
    /// without slowing apply down.)
    ///
    /// Empty = secrets feature off.
    #[serde(default)]
    pub recipients: Vec<String>,

    /// Vault provider config — when set, `yui secret store` /
    /// `yui secret unlock` use it to ferry the X25519 identity
    /// across machines via Bitwarden / 1Password instead of
    /// asking the user to copy `~/.config/yui/age.txt` by hand.
    /// Off when absent.
    #[serde(default)]
    pub vault: Option<VaultConfig>,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            identity: default_identity_path(),
            recipients: Vec::new(),
            vault: None,
        }
    }
}

/// `[secrets.vault]` — points yui at a vault item that holds the
/// X25519 identity. yui doesn't authenticate against the vault
/// itself; it shells out to the provider's official CLI (`bw` or
/// `op`), which already knows how to drive its own auth flow
/// (master password, biometric, passkey-via-web-vault, SSO).
///
/// Storage convention: the X25519 secret file's full content
/// (header comments + the `AGE-SECRET-KEY-1…` line) goes in the
/// item's notes field. Picking notes (rather than the password
/// field) keeps the multi-line content intact and doesn't pollute
/// the vault's password autofill UI.
#[derive(Debug, Clone, Deserialize)]
pub struct VaultConfig {
    /// `"bitwarden"` or `"1password"`.
    pub provider: VaultProvider,
}

/// Vault item name yui stores the X25519 identity under. Hardcoded
/// rather than configurable — the realistic "I have multiple yui
/// dotfiles trees sharing one vault account" case is rare enough
/// that the simplification of one-less config knob wins. Add a
/// configurable knob back if a user actually hits the collision.
pub const VAULT_ITEM_NAME: &str = "yui-x25519-identity";

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VaultProvider {
    Bitwarden,
    #[serde(alias = "1password")]
    OnePassword,
}

impl SecretsConfig {
    /// `[secrets]` is "on" once the user has populated `recipients`
    /// (which `yui secret init` does). Until then, the apply walker
    /// won't even look for `*.age` files — keeps every existing
    /// dotfiles repo behaving exactly the same as before this PR.
    pub fn enabled(&self) -> bool {
        !self.recipients.is_empty()
    }
}

fn default_identity_path() -> String {
    // Cross-platform `~/.config/yui/age.txt` — `paths::expand_tilde`
    // turns `~` into `$HOME` / `$USERPROFILE` at use time so the
    // string stays portable across machines.
    "~/.config/yui/age.txt".to_string()
}

fn default_ts_format() -> String {
    "%Y%m%d_%H%M%S%3f".to_string()
}

fn default_true() -> bool {
    true
}

/// Load + merge config files from `$DOTFILES`.
pub fn load(source: &Utf8Path, yui: &YuiVars) -> Result<Config> {
    let files = list_config_files(source)?;
    if files.is_empty() {
        return Err(Error::Config(format!(
            "no config.toml / config.*.toml found at {source}"
        )));
    }

    let mut engine = template::Engine::new();
    let mut merged = toml::Table::new();
    let mut vars_acc = toml::Table::new();

    for file in &files {
        let raw = std::fs::read_to_string(file)
            .map_err(|e| Error::Config(format!("read {file}: {e}")))?;

        // Pre-extract this file's own `[vars]` section as plain text and
        // merge it into `vars_acc` BEFORE rendering. Without this, a
        // file's `[[mount.entry]] dst = "{{ vars.home_root }}"` couldn't
        // reference a `home_root` declared at the top of the same file
        // — it would only see vars from previously-loaded files.
        if let Some(file_vars) = pre_extract_vars(&raw, file)? {
            deep_merge_table(&mut vars_acc, file_vars);
        }
        // Resolve cross-references within vars (`a = "{{ b }}"`,
        // `b = "raw"` — possibly across files) by iteratively rendering
        // every string value in `vars_acc` with `vars_acc` itself as
        // the context, until nothing changes (or we've burned through
        // the iteration budget — that catches genuine cycles).
        resolve_vars_refs(&mut vars_acc, yui, &mut engine)?;

        // Use the config-flavoured context so hook-level placeholders
        // (`{{ script_path }}` etc.) survive this pass intact. Dotfile
        // rendering keeps the bare `template_context`.
        let ctx = template::config_render_context(yui, &vars_acc);
        let rendered = engine.render(&raw, &ctx)?;
        let parsed: toml::Table =
            toml::from_str(&rendered).map_err(|e| Error::Config(format!("parse {file}: {e}")))?;

        // Re-merge vars from the parsed (Tera-rendered) form. Pre-extract
        // gives us the unrendered shape; the rendered form may have
        // resolved `{{ env(...) }}` etc. and we want those resolved
        // values visible to subsequent files.
        if let Some(toml::Value::Table(file_vars)) = parsed.get("vars") {
            deep_merge_table(&mut vars_acc, file_vars.clone());
        }
        deep_merge_table(&mut merged, parsed);
    }

    let cfg: Config = toml::Value::Table(merged)
        .try_into()
        .map_err(|e| Error::Config(format!("schema: {e}")))?;

    // Surface the legacy-alias deprecation warning once, here at load
    // time, rather than as a side effect of the `update_mode` resolver.
    // This keeps the resolver pure and avoids duplicate warnings.
    cfg.ui.warn_deprecated_auto_update_check();

    Ok(cfg)
}

/// Pull just the `[vars]` (and `[vars.X]` sub-tables) out of a config
/// file's raw text and parse them as standalone TOML, ignoring the
/// rest. Returns `None` when the file has no `[vars]` section.
///
/// Skips Tera control blocks (`{% ... %}` lines) so a file using
/// `{% set ... %}` at the top doesn't break the extraction. Any value
/// inside `[vars]` that itself contains Tera (`{{ ... }}` or `{% ... %}`)
/// would round-trip through TOML deserialization unchanged — Tera
/// rendering is the second pass.
fn pre_extract_vars(raw: &str, file: &Utf8Path) -> Result<Option<toml::Table>> {
    let mut in_vars = false;
    let mut found_vars = false;
    let mut lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        // Strip a trailing comment so a section header like
        // `[options]  # group` still ends the [vars] capture.
        let header = trimmed.split('#').next().unwrap_or("").trim();
        if header.starts_with("[") {
            // Section start. `[vars]` or `[vars.<X>]` opens / continues
            // the capture; anything else closes it.
            let normalized: String = header.chars().filter(|c| !c.is_whitespace()).collect();
            if normalized == "[vars]"
                || normalized.starts_with("[vars.")
                || normalized.starts_with("[vars[")
            {
                in_vars = true;
                found_vars = true;
                lines.push(line);
                continue;
            }
            in_vars = false;
            continue;
        }
        // Tera control block at column 0 — skip so the standalone
        // TOML parse doesn't see `{% set ... %}` and choke. Inline
        // `{{ ... }}` inside values is fine because TOML happily
        // accepts them as plain strings.
        if trimmed.starts_with("{%") {
            continue;
        }
        if in_vars {
            lines.push(line);
        }
    }
    if !found_vars {
        return Ok(None);
    }
    let extracted = lines.join("\n");
    let parsed: toml::Table = toml::from_str(&extracted).map_err(|e| {
        Error::Config(format!(
            "pre-extract [vars] from {file}: {e} \
             (the [vars] block must be parseable on its own — \
             move computed values into a `set` block above the section)"
        ))
    })?;
    if let Some(toml::Value::Table(vars)) = parsed.get("vars") {
        Ok(Some(vars.clone()))
    } else {
        Ok(None)
    }
}

/// Maximum number of resolution iterations. Each iteration evaluates
/// every templated string value in `vars` with the current vars as the
/// context. Genuine cycles (`a = "{{ b }}"`, `b = "{{ a }}"`) hit this
/// budget and bail out — leaving the values as-is rather than looping
/// forever or panicking.
const MAX_VARS_RESOLVE_ITERATIONS: usize = 8;

/// Iteratively Tera-render every string value in a vars table using the
/// vars table itself (plus `yui.*` / `env(…)`) as the rendering context,
/// until no value changes between iterations.
fn resolve_vars_refs(
    vars: &mut toml::Table,
    yui: &YuiVars,
    engine: &mut template::Engine,
) -> Result<()> {
    for _ in 0..MAX_VARS_RESOLVE_ITERATIONS {
        // `config_render_context` for parity with the main config
        // render pass — a vars value that happens to include
        // `{{ script_path }}` should pass through here for the same
        // reason it does at the file level.
        let ctx = template::config_render_context(yui, vars);
        let mut changed = false;
        render_strings_in_table(vars, engine, &ctx, &mut changed)?;
        if !changed {
            return Ok(());
        }
    }
    // Hit the budget — likely a cycle. We leave the partially-resolved
    // values in place (rather than erroring) so the rest of yui keeps
    // working; downstream Tera renders will surface a useful error if
    // the unresolved value lands somewhere it matters.
    Ok(())
}

fn render_strings_in_table(
    table: &mut toml::Table,
    engine: &mut template::Engine,
    ctx: &teravars::Context,
    changed: &mut bool,
) -> Result<()> {
    for (_k, value) in table.iter_mut() {
        render_strings_in_value(value, engine, ctx, changed)?;
    }
    Ok(())
}

fn render_strings_in_value(
    value: &mut toml::Value,
    engine: &mut template::Engine,
    ctx: &teravars::Context,
    changed: &mut bool,
) -> Result<()> {
    match value {
        toml::Value::String(s) => {
            if !s.contains("{{") && !s.contains("{%") {
                return Ok(());
            }
            let rendered = engine.render(s.as_str(), ctx)?;
            if rendered != *s {
                *s = rendered;
                *changed = true;
            }
        }
        toml::Value::Table(t) => {
            render_strings_in_table(t, engine, ctx, changed)?;
        }
        toml::Value::Array(arr) => {
            for v in arr.iter_mut() {
                render_strings_in_value(v, engine, ctx, changed)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// List config files in merge order:
///   `config.toml` (rank 0)
/// → `config.*.toml` alphabetically (rank 1, excluding `config.local.toml`)
/// → `config.local.toml` (rank 2, last/highest priority)
fn list_config_files(source: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let entries =
        std::fs::read_dir(source).map_err(|e| Error::Config(format!("read_dir {source}: {e}")))?;
    let mut files: Vec<Utf8PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Error::Io)?;
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        let is_match = name == "config.toml"
            || (name.starts_with("config.") && name.ends_with(".toml") && name.len() > 12);
        if !is_match {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| Error::Config(format!("non-UTF8 config path: {}", p.display())))?;
        files.push(path);
    }
    files.sort_by(|a, b| {
        let an = a.file_name().unwrap_or("");
        let bn = b.file_name().unwrap_or("");
        file_rank(an).cmp(&file_rank(bn)).then_with(|| an.cmp(bn))
    });
    Ok(files)
}

fn file_rank(name: &str) -> u8 {
    match name {
        "config.toml" => 0,
        "config.local.toml" => 2,
        _ => 1,
    }
}

/// Deep-merge `overlay` into `base`. Tables recurse; arrays append; scalars
/// overlay-wins.
fn deep_merge_table(base: &mut toml::Table, overlay: toml::Table) {
    for (k, v) in overlay {
        match (base.remove(&k), v) {
            (Some(toml::Value::Table(mut bt)), toml::Value::Table(ot)) => {
                deep_merge_table(&mut bt, ot);
                base.insert(k, toml::Value::Table(bt));
            }
            (Some(toml::Value::Array(mut ba)), toml::Value::Array(oa)) => {
                ba.extend(oa);
                base.insert(k, toml::Value::Array(ba));
            }
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn yui_vars(source: &Utf8Path) -> YuiVars {
        YuiVars {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: "test".into(),
            user: "u".into(),
            source: source.to_string(),
        }
    }

    fn write(tmp: &TempDir, name: &str, body: &str) {
        std::fs::write(tmp.path().join(name), body).unwrap();
    }

    fn root(tmp: &TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap()
    }

    #[test]
    fn loads_single_file() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "config.toml",
            r#"
[vars]
git_email = "a@example.com"

[[mount.entry]]
src = "home"
dst = "/home/u"
"#,
        );
        let r = root(&tmp);
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert_eq!(
            cfg.vars.get("git_email").unwrap().as_str(),
            Some("a@example.com")
        );
        assert_eq!(cfg.mount.entry.len(), 1);
        assert_eq!(cfg.mount.entry[0].dst, "/home/u");
    }

    #[test]
    fn local_overrides_base() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "config.toml",
            r#"
[vars]
git_email = "a@example.com"
work_mode = false
"#,
        );
        write(
            &tmp,
            "config.local.toml",
            r#"
[vars]
git_email = "b@work.com"
"#,
        );
        let r = root(&tmp);
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert_eq!(
            cfg.vars.get("git_email").unwrap().as_str(),
            Some("b@work.com")
        );
        // unchanged keys preserved
        assert_eq!(cfg.vars.get("work_mode").unwrap().as_bool(), Some(false));
    }

    #[test]
    fn alphabetical_middle_files_apply_after_base_before_local() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "config.toml",
            r#"[vars]
val = "base""#,
        );
        write(
            &tmp,
            "config.aaa.toml",
            r#"[vars]
val = "aaa""#,
        );
        write(
            &tmp,
            "config.zzz.toml",
            r#"[vars]
val = "zzz""#,
        );
        write(
            &tmp,
            "config.local.toml",
            r#"[vars]
val = "local""#,
        );
        let r = root(&tmp);
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert_eq!(cfg.vars.get("val").unwrap().as_str(), Some("local"));
    }

    #[test]
    fn yui_vars_available_in_render() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "config.toml",
            r#"
[[mount.entry]]
src = "home"
dst = "/{{ yui.os }}/dst"
"#,
        );
        let r = root(&tmp);
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert_eq!(cfg.mount.entry[0].dst, "/linux/dst");
    }

    #[test]
    fn mount_entries_append_across_files() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "config.toml",
            r#"
[[mount.entry]]
src = "home"
dst = "/h"
"#,
        );
        write(
            &tmp,
            "config.local.toml",
            r#"
[[mount.entry]]
src = "appdata"
dst = "/a"
"#,
        );
        let r = root(&tmp);
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert_eq!(cfg.mount.entry.len(), 2);
    }

    #[test]
    fn missing_config_errors() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        let err = load(&r, &yui_vars(&r)).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn defaults_apply_when_sections_absent() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "config.toml", "");
        let r = root(&tmp);
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert!(cfg.absorb.auto);
        assert!(cfg.absorb.require_clean_git);
        assert!(cfg.render.manage_gitignore);
        assert_eq!(cfg.backup.dir, ".yui/backup");
        assert_eq!(cfg.mount.marker_filename, ".yuilink");
    }

    /// Pre-extract: a value declared in `[vars]` should be visible to
    /// other sections of the same file during Tera rendering. Without
    /// pre-extract this would fail because the file's own vars aren't
    /// added to the context until AFTER rendering.
    #[test]
    fn vars_visible_to_same_file_render() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "config.toml",
            r#"
[vars]
home_root = "/custom/home"

[[mount.entry]]
src = "home"
dst = "{{ vars.home_root }}"
"#,
        );
        let r = root(&tmp);
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert_eq!(cfg.mount.entry.len(), 1);
        assert_eq!(cfg.mount.entry[0].dst, "/custom/home");
    }

    /// Tera `set` blocks at the top of the file (used by some configs
    /// for computed values) shouldn't break the standalone TOML parse
    /// of the [vars] block that lives further down.
    #[test]
    fn vars_extract_skips_set_blocks() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "config.toml",
            r#"
{% set computed = "abc" %}
[vars]
plain = "real"

[[mount.entry]]
src = "home"
dst = "{{ vars.plain }}"
"#,
        );
        let r = root(&tmp);
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert_eq!(cfg.mount.entry[0].dst, "real");
    }

    /// Vars that reference other vars should resolve regardless of
    /// declaration order (the resolver iterates until convergence).
    #[test]
    fn vars_cross_reference_resolves_either_order() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "config.toml",
            r#"
[vars]
a = "{{ vars.b }}"
b = "raw"

[[mount.entry]]
src = "home"
dst = "{{ vars.a }}"
"#,
        );
        let r = root(&tmp);
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert_eq!(cfg.mount.entry[0].dst, "raw");
    }

    /// Genuine cycles (`a = {{b}}` + `b = {{a}}`) shouldn't loop or
    /// panic. The resolver bails after the iteration budget and leaves
    /// the values as-is; downstream Tera renders that hit the
    /// unresolved value will surface a clear error if it actually
    /// matters at that site.
    #[test]
    fn vars_cycle_does_not_loop_forever() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "config.toml",
            r#"
[vars]
a = "{{ vars.b }}"
b = "{{ vars.a }}"

[[mount.entry]]
src = "home"
dst = "/anywhere"
"#,
        );
        let r = root(&tmp);
        // Loads without panicking. The unresolved a/b just stay as
        // literal Tera strings; load() succeeds because no other
        // section actually references them.
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert_eq!(cfg.mount.entry[0].dst, "/anywhere");
    }

    /// Hook-level Tera tokens (`{{ script_path }}` etc.) must survive
    /// the config-load render verbatim — otherwise every author would
    /// have to wrap them in `{% raw %}{% endraw %}`. The placeholders
    /// are seeded as self-references in `template_context` so Tera
    /// just emits them back; the hook executor's
    /// `build_hook_context` overrides them with real paths at run
    /// time.
    #[test]
    fn hook_script_vars_survive_config_load_render_verbatim() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "config.toml",
            r#"
[[mount.entry]]
src = "home"
dst = "/home/u"

[[hook]]
name = "deno-build"
script = ".yui/bin/build.ts"
command = "deno"
args = ["run", "-A", "{{ script_path }}"]
when_run = "onchange"
"#,
        );
        let r = root(&tmp);
        let cfg = load(&r, &yui_vars(&r)).unwrap();
        assert_eq!(cfg.hook.len(), 1);
        // The args literal made it through config-load untouched —
        // the third arg is `{{ script_path }}`, ready for the hook
        // executor to render with the real path.
        assert_eq!(cfg.hook[0].args, vec!["run", "-A", "{{ script_path }}"]);
    }

    // ========================================================
    // [ui] auto_update mode resolution (kaishin 0.5.0)
    // ========================================================

    fn ui(body: &str) -> UiConfig {
        toml::from_str::<UiConfig>(body).unwrap()
    }

    #[test]
    fn auto_update_defaults_to_install() {
        let u = ui("");
        assert_eq!(u.auto_update, None);
        assert_eq!(u.auto_update_check, None);
        assert_eq!(u.update_mode(), AutoUpdateMode::Install);
    }

    #[test]
    fn auto_update_explicit_off() {
        let u = ui(r#"auto_update = "off""#);
        assert_eq!(u.auto_update, Some(AutoUpdateMode::Off));
        assert_eq!(u.update_mode(), AutoUpdateMode::Off);
    }

    #[test]
    fn auto_update_explicit_notify() {
        let u = ui(r#"auto_update = "notify""#);
        assert_eq!(u.auto_update, Some(AutoUpdateMode::Notify));
        assert_eq!(u.update_mode(), AutoUpdateMode::Notify);
    }

    #[test]
    fn auto_update_explicit_install() {
        let u = ui(r#"auto_update = "install""#);
        assert_eq!(u.auto_update, Some(AutoUpdateMode::Install));
        assert_eq!(u.update_mode(), AutoUpdateMode::Install);
    }

    #[test]
    fn deprecated_auto_update_check_false_maps_to_off() {
        let u = ui("auto_update_check = false");
        assert_eq!(u.auto_update_check, Some(false));
        assert_eq!(u.update_mode(), AutoUpdateMode::Off);
    }

    #[test]
    fn deprecated_auto_update_check_true_maps_to_notify() {
        let u = ui("auto_update_check = true");
        assert_eq!(u.auto_update_check, Some(true));
        assert_eq!(u.update_mode(), AutoUpdateMode::Notify);
    }

    #[test]
    fn explicit_auto_update_wins_over_deprecated_bool() {
        // user set both: the explicit `auto_update` always wins.
        let u = ui(r#"auto_update = "off"
auto_update_check = true"#);
        assert_eq!(u.update_mode(), AutoUpdateMode::Off);

        let u = ui(r#"auto_update = "notify"
auto_update_check = false"#);
        assert_eq!(u.update_mode(), AutoUpdateMode::Notify);

        // The case the `Option` exists for: an explicit
        // `auto_update = "install"` is distinguishable from the omitted
        // default and so beats a stale `auto_update_check = false`
        // (which would otherwise force `off`).
        let u = ui(r#"auto_update = "install"
auto_update_check = false"#);
        assert_eq!(u.auto_update, Some(AutoUpdateMode::Install));
        assert_eq!(u.update_mode(), AutoUpdateMode::Install);
    }

    #[test]
    fn update_check_interval_still_parses() {
        let u = ui(r#"update_check_interval = "12h""#);
        assert_eq!(u.update_check_interval.as_deref(), Some("12h"));
    }
}
