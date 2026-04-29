//! Hook system — run user-supplied scripts around `yui apply`.
//!
//! Scripts live at `$DOTFILES/<config.script>` (idiomatic place is
//! `.yui/bin/<name>.sh`). They're plain executables — no yui imports,
//! no special protocol. yui just decides *when* to invoke them based on
//! the `[[hook]]` config and the persisted state file.
//!
//! ## State
//!
//! Per-hook outcomes are stored in `$DOTFILES/.yui/state.json`:
//!
//! ```json
//! {
//!   "version": 1,
//!   "hooks": {
//!     "install-tools": {
//!       "last_run_at": "2026-04-29T08:30:00+09:00[Asia/Tokyo]",
//!       "last_content_hash": "sha256:abc123..."
//!     }
//!   }
//! }
//! ```
//!
//! `last_run_at` is filled on every successful run; `last_content_hash`
//! is only filled for `when_run = "onchange"` hooks.

use std::collections::BTreeMap;
use std::process::Command;

use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tera::Context as TeraContext;
use tracing::info;

use crate::config::{Config, HookConfig, HookPhase, WhenRun};
use crate::template::{self, Engine};
use crate::vars::YuiVars;
use crate::{Error, Result};

const STATE_REL_PATH: &str = ".yui/state.json";
const STATE_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub hooks: BTreeMap<String, HookState>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HookState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_content_hash: Option<String>,
}

impl State {
    pub fn load(source: &Utf8Path) -> Result<Self> {
        let path = source.join(STATE_REL_PATH);
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                serde_json::from_str(&s).map_err(|e| Error::Config(format!("parse {path}: {e}")))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Atomically persist the state to disk: write to a sibling `.tmp`
    /// file then rename, so an interrupted save can't leave a half-
    /// written state.json behind. (gemini medium PR #20)
    pub fn save(&self, source: &Utf8Path) -> Result<()> {
        let path = source.join(STATE_REL_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut body = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("serialize state: {e}")))?;
        body.push('\n');
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// What happened when we considered running a hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// The hook ran and exited successfully.
    Ran,
    /// `when_run = "once"` and the hook has run before.
    SkippedOnce,
    /// `when_run = "onchange"` and the script's hash matches state.
    SkippedUnchanged,
    /// `when` evaluated false on this host.
    SkippedWhenFalse,
    /// `dry_run = true` — the hook would have run.
    DryRun,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

fn now_iso8601() -> String {
    jiff::Zoned::now().to_string()
}

/// Build a Tera context for a hook: standard `template_context` + the
/// `script_*` vars that `command` / `args` can interpolate.
pub fn build_hook_context(
    yui: &YuiVars,
    vars: &toml::Table,
    script_path: &Utf8Path,
) -> TeraContext {
    let mut ctx = template::template_context(yui, vars);
    ctx.insert("script_path", &script_path.as_str());
    ctx.insert(
        "script_dir",
        &script_path.parent().map(|p| p.as_str()).unwrap_or(""),
    );
    ctx.insert("script_name", &script_path.file_name().unwrap_or(""));
    ctx.insert("script_stem", &script_path.file_stem().unwrap_or(""));
    ctx.insert("script_ext", &script_path.extension().unwrap_or(""));
    ctx
}

/// Decide whether to run `hook` and run it if so. Updates `state` in
/// memory on a successful run — caller is responsible for persisting
/// (typically via `state.save(source)` after each `Ran` outcome, so a
/// later hook failure doesn't lose the record of the earlier success).
///
/// `force = true` bypasses the `when_run` state check (still respects
/// `when` — an explicit `yui hooks run <name>` shouldn't suddenly run a
/// hook that's `when = "yui.os == 'macos'"` on Linux).
#[allow(clippy::too_many_arguments)]
pub fn run_hook(
    hook: &HookConfig,
    source: &Utf8Path,
    yui: &YuiVars,
    vars: &toml::Table,
    engine: &mut Engine,
    base_ctx: &TeraContext,
    state: &mut State,
    dry_run: bool,
    force: bool,
) -> Result<HookOutcome> {
    if let Some(when) = &hook.when {
        if !template::eval_truthy(when, engine, base_ctx)? {
            return Ok(HookOutcome::SkippedWhenFalse);
        }
    }

    let script_path = source.join(&hook.script);

    // Hash the script body only when the `when_run` policy actually
    // uses the value (`Onchange`). For `Once` / `Every` we don't need
    // the hash either for the decision OR for state, so skip the
    // read+SHA. (gemini medium PR #20)
    let current_hash = if hook.when_run == WhenRun::Onchange {
        match std::fs::read(&script_path) {
            Ok(bytes) => Some(sha256_hex(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        }
    } else {
        None
    };

    if !force {
        let prior = state.hooks.get(&hook.name);
        match hook.when_run {
            WhenRun::Once => {
                if prior.and_then(|s| s.last_run_at.as_ref()).is_some() {
                    return Ok(HookOutcome::SkippedOnce);
                }
            }
            WhenRun::Onchange => {
                if let (Some(prior_state), Some(now_hash)) = (prior, current_hash.as_deref()) {
                    if prior_state.last_content_hash.as_deref() == Some(now_hash) {
                        return Ok(HookOutcome::SkippedUnchanged);
                    }
                }
            }
            WhenRun::Every => {}
        }
    }

    // Validate that the script exists *before* the dry-run early
    // return — `apply --dry-run` should still surface a missing script
    // as an error rather than silently reporting "would run". (gemini
    // medium PR #20)
    if !script_path.is_file() {
        return Err(Error::Other(anyhow::anyhow!(
            "hook[{}]: script not found at {script_path}",
            hook.name
        )));
    }

    if dry_run {
        return Ok(HookOutcome::DryRun);
    }

    let hook_ctx = build_hook_context(yui, vars, &script_path);
    let command = engine.render(&hook.command, &hook_ctx)?;
    let args: Vec<String> = hook
        .args
        .iter()
        .map(|a| engine.render(a, &hook_ctx))
        .collect::<Result<_>>()?;

    info!(
        "hook[{}] running: {} {}",
        hook.name,
        command,
        args.join(" ")
    );
    let status = Command::new(&command)
        .args(&args)
        .current_dir(source.as_std_path())
        .status()
        .map_err(|e| Error::Other(anyhow::anyhow!("hook[{}]: spawn {command}: {e}", hook.name)))?;

    if !status.success() {
        return Err(Error::Other(anyhow::anyhow!(
            "hook[{}] exited with status {status}",
            hook.name
        )));
    }

    state.version = STATE_VERSION;
    state.hooks.insert(
        hook.name.clone(),
        HookState {
            last_run_at: Some(now_iso8601()),
            last_content_hash: current_hash,
        },
    );

    Ok(HookOutcome::Ran)
}

/// Run every hook whose phase matches. Stops at the first failure (the
/// user can investigate, fix, and re-run; we don't want to silently keep
/// going after a failed `pre` hook).
pub fn run_phase(
    config: &Config,
    source: &Utf8Path,
    yui: &YuiVars,
    engine: &mut Engine,
    base_ctx: &TeraContext,
    phase: HookPhase,
    dry_run: bool,
) -> Result<()> {
    let mut state = State::load(source)?;
    for hook in &config.hook {
        if hook.phase != phase {
            continue;
        }
        let outcome = run_hook(
            hook,
            source,
            yui,
            &config.vars,
            engine,
            base_ctx,
            &mut state,
            dry_run,
            /* force */ false,
        )?;
        let phase_name = match phase {
            HookPhase::Pre => "pre",
            HookPhase::Post => "post",
        };
        info!("hook[{}] {phase_name}: {:?}", hook.name, outcome);
        // Save after each successful run so a later failure doesn't
        // discard the earlier successes.
        if outcome == HookOutcome::Ran {
            state.save(source)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn utf8(p: std::path::PathBuf) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(p).unwrap()
    }

    fn yui_vars(source: &Utf8Path) -> YuiVars {
        YuiVars {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            host: "test".into(),
            user: "u".into(),
            source: source.to_string(),
        }
    }

    #[test]
    fn state_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().to_path_buf());
        let state = State {
            version: STATE_VERSION,
            hooks: BTreeMap::from([(
                "h1".to_string(),
                HookState {
                    last_run_at: Some("2026-04-29T00:00:00Z".into()),
                    last_content_hash: Some("sha256:abc".into()),
                },
            )]),
        };
        state.save(&source).unwrap();
        let reloaded = State::load(&source).unwrap();
        assert_eq!(reloaded.version, STATE_VERSION);
        assert_eq!(
            reloaded
                .hooks
                .get("h1")
                .unwrap()
                .last_content_hash
                .as_deref(),
            Some("sha256:abc")
        );
    }

    #[test]
    fn state_load_returns_default_when_absent() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().to_path_buf());
        let s = State::load(&source).unwrap();
        assert_eq!(s.version, 0);
        assert!(s.hooks.is_empty());
    }

    #[test]
    fn sha256_hex_format_includes_prefix() {
        let h = sha256_hex(b"hello");
        assert!(h.starts_with("sha256:"));
        assert_eq!(h.len(), 7 + 64); // "sha256:" + 64 hex chars
    }

    fn make_engine_and_ctx(source: &Utf8Path, vars: &toml::Table) -> (Engine, TeraContext) {
        let engine = Engine::new();
        let ctx = template::template_context(&yui_vars(source), vars);
        (engine, ctx)
    }

    fn write_script(source: &Utf8Path, rel: &str, body: &str) -> Utf8PathBuf {
        let path = source.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    /// Helper that wraps run_hook + a freshly-loaded state. Used by all
    /// the test cases below — the production callers (`run_phase`,
    /// `cmd::hooks_run`) reuse one State across multiple hooks, but
    /// per-test isolation is fine here.
    #[allow(clippy::too_many_arguments)]
    fn run_hook_test(
        hook: &HookConfig,
        source: &Utf8Path,
        yui: &YuiVars,
        vars: &toml::Table,
        engine: &mut Engine,
        ctx: &TeraContext,
        dry_run: bool,
        force: bool,
    ) -> Result<HookOutcome> {
        let mut state = State::load(source)?;
        let outcome = run_hook(
            hook, source, yui, vars, engine, ctx, &mut state, dry_run, force,
        )?;
        if outcome == HookOutcome::Ran {
            state.save(source)?;
        }
        Ok(outcome)
    }

    /// Most tests need only a `bash` hook; this builds one with the
    /// usual defaults. Caller picks the `when_run`.
    fn bash_hook(when_run: WhenRun, when: Option<&str>) -> HookConfig {
        HookConfig {
            name: "h".into(),
            script: ".yui/bin/h.sh".into(),
            command: "bash".into(),
            args: vec!["{{ script_path }}".into()],
            when_run,
            phase: HookPhase::Post,
            when: when.map(str::to_string),
        }
    }

    #[test]
    fn dry_run_returns_dry_run_outcome() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().to_path_buf());
        // Script must exist — dry_run now validates existence.
        write_script(&source, ".yui/bin/h.sh", "#!/bin/sh\nexit 0\n");
        let hook = bash_hook(WhenRun::Every, None);
        let vars = toml::Table::new();
        let (mut engine, ctx) = make_engine_and_ctx(&source, &vars);
        let outcome = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            /* dry_run */ true,
            /* force */ false,
        )
        .unwrap();
        assert_eq!(outcome, HookOutcome::DryRun);
        assert!(!source.join(STATE_REL_PATH).exists());
    }

    #[test]
    fn dry_run_errors_when_script_missing() {
        // Even in dry-run we should validate that the script exists —
        // otherwise `apply --dry-run` would happily report "would run"
        // for a misconfigured hook. (gemini PR #20 medium #4.)
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().to_path_buf());
        let hook = bash_hook(WhenRun::Every, None);
        let vars = toml::Table::new();
        let (mut engine, ctx) = make_engine_and_ctx(&source, &vars);
        let err = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            /* dry_run */ true,
            /* force */ false,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("script not found"));
    }

    #[test]
    fn when_false_skips_without_running() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().to_path_buf());
        write_script(&source, ".yui/bin/h.sh", "#!/bin/sh\nexit 1\n"); // would fail if run
        let hook = bash_hook(WhenRun::Every, Some("yui.os == 'no-such-os'"));
        let vars = toml::Table::new();
        let (mut engine, ctx) = make_engine_and_ctx(&source, &vars);
        let outcome = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            /* dry_run */ false,
            /* force */ false,
        )
        .unwrap();
        assert_eq!(outcome, HookOutcome::SkippedWhenFalse);
        assert!(!source.join(STATE_REL_PATH).exists());
    }

    #[test]
    fn force_still_respects_when_filter() {
        // --force bypasses the time/hash state check, but the OS gate
        // (`when = "yui.os == 'no-such-os'"`) is a real config filter
        // that should still keep the hook from running.
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().to_path_buf());
        write_script(&source, ".yui/bin/h.sh", "#!/bin/sh\nexit 1\n");
        let hook = bash_hook(WhenRun::Every, Some("yui.os == 'no-such-os'"));
        let vars = toml::Table::new();
        let (mut engine, ctx) = make_engine_and_ctx(&source, &vars);
        let outcome = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            false,
            /* force */ true,
        )
        .unwrap();
        assert_eq!(outcome, HookOutcome::SkippedWhenFalse);
    }

    // The remaining tests actually spawn `bash` so they're Unix-only —
    // Windows GitHub runners don't have bash on $PATH (and we don't
    // want to tie the test suite to Git Bash's path on Windows). The
    // production code is portable; the tests just pick a portable
    // command set when CI matters.

    #[cfg(unix)]
    #[test]
    fn once_runs_first_then_skips() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().to_path_buf());
        let marker = source.join(".ran");
        write_script(
            &source,
            ".yui/bin/h.sh",
            &format!("#!/bin/sh\necho ok > {:?}\n", marker.as_str()),
        );
        let hook = bash_hook(WhenRun::Once, None);
        let vars = toml::Table::new();
        let (mut engine, ctx) = make_engine_and_ctx(&source, &vars);

        let first = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            false,
            false,
        )
        .unwrap();
        assert_eq!(first, HookOutcome::Ran);
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();

        let second = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            false,
            false,
        )
        .unwrap();
        assert_eq!(second, HookOutcome::SkippedOnce);
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn onchange_runs_when_hash_differs() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().to_path_buf());
        let marker = source.join(".ran");
        let script = source.join(".yui/bin/h.sh");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        let body_v1 = format!("#!/bin/sh\necho v1 > {:?}\n", marker.as_str());
        std::fs::write(&script, &body_v1).unwrap();
        let hook = bash_hook(WhenRun::Onchange, None);
        let vars = toml::Table::new();
        let (mut engine, ctx) = make_engine_and_ctx(&source, &vars);

        let first = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            false,
            false,
        )
        .unwrap();
        assert_eq!(first, HookOutcome::Ran);
        std::fs::remove_file(&marker).unwrap();

        let second = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            false,
            false,
        )
        .unwrap();
        assert_eq!(second, HookOutcome::SkippedUnchanged);
        assert!(!marker.exists());

        let body_v2 = format!("#!/bin/sh\necho v2 > {:?}\n", marker.as_str());
        std::fs::write(&script, &body_v2).unwrap();
        let third = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            false,
            false,
        )
        .unwrap();
        assert_eq!(third, HookOutcome::Ran);
        assert!(marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn force_bypasses_state_check() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().to_path_buf());
        let marker = source.join(".ran");
        let script = source.join(".yui/bin/h.sh");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho hi >> {:?}\n", marker.as_str()),
        )
        .unwrap();
        let hook = bash_hook(WhenRun::Once, None);
        let vars = toml::Table::new();
        let (mut engine, ctx) = make_engine_and_ctx(&source, &vars);

        let _ = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            false,
            false,
        )
        .unwrap();
        let forced = run_hook_test(
            &hook,
            &source,
            &yui_vars(&source),
            &vars,
            &mut engine,
            &ctx,
            false,
            /* force */ true,
        )
        .unwrap();
        assert_eq!(forced, HookOutcome::Ran);
        let body = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(body.lines().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn run_phase_saves_after_each_success() {
        // Two `every` hooks; both run, both records persist in
        // state.json — verifying we save inside the loop, not at end.
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().to_path_buf());
        write_script(&source, ".yui/bin/a.sh", "#!/bin/sh\nexit 0\n");
        write_script(&source, ".yui/bin/b.sh", "#!/bin/sh\nexit 0\n");
        let cfg = Config {
            hook: vec![
                HookConfig {
                    name: "a".into(),
                    script: ".yui/bin/a.sh".into(),
                    command: "bash".into(),
                    args: vec!["{{ script_path }}".into()],
                    when_run: WhenRun::Every,
                    phase: HookPhase::Post,
                    when: None,
                },
                HookConfig {
                    name: "b".into(),
                    script: ".yui/bin/b.sh".into(),
                    command: "bash".into(),
                    args: vec!["{{ script_path }}".into()],
                    when_run: WhenRun::Every,
                    phase: HookPhase::Post,
                    when: None,
                },
            ],
            ..Default::default()
        };
        let yui = yui_vars(&source);
        let mut engine = Engine::new();
        let ctx = template::template_context(&yui, &cfg.vars);
        run_phase(
            &cfg,
            &source,
            &yui,
            &mut engine,
            &ctx,
            HookPhase::Post,
            false,
        )
        .unwrap();
        let state = State::load(&source).unwrap();
        assert!(state.hooks.contains_key("a"));
        assert!(state.hooks.contains_key("b"));
    }
}
