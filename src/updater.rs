//! Self-update support for yui, using the shared `kaishin` library.
//!
//! Facade around kaishin's async API. `run_self_update` drives it
//! from a blocking `current_thread` runtime so that synchronous path
//! stays simple; the background auto-update instead spawns a `tokio`
//! task on `main`'s small `new_multi_thread` runtime so the fetch /
//! install overlaps the command, then drains it at shutdown with a
//! short, bounded `tokio::time::timeout`. Same shape as renri's
//! `src/updater.rs`; the only yui-specific bit is the hardcoded
//! `(owner, repo, bin)` triple — yui's crate name is `yui-cli`
//! (because crates.io's
//! `yui` is taken by an unrelated abandoned crate), but the repo
//! and binary are both `yui`, so going through `env!("CARGO_PKG_NAME")`
//! the way renri does would produce the wrong GitHub Release URL.
//!
//! The module exposes two layers:
//!
//! - [`run_self_update`] — drives the `yui self-update` subcommand
//!   (interactive / `--yes` / `--check`).
//! - [`Checker`] + [`maybe_spawn_auto_update_check`] /
//!   [`finalize_auto_update_check`] — the daily background
//!   auto-update run after every other subcommand, the way `rvpm` /
//!   `renri` do it. `[ui] auto_update` in `config.toml` picks the
//!   mode (`off` / `notify` / `install`, default `install` = silent
//!   background install applied on next launch), and `[ui]
//!   update_check_interval = "..."` overrides the default 24h
//!   cadence. The `YUI_NO_AUTOUPDATE` env var is a kill-switch that
//!   disables all of it regardless of config.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

use crate::config;
use crate::config::AutoUpdateMode;
use crate::paths;
use crate::vars::YuiVars;

const OWNER: &str = "yukimemi";
const REPO: &str = "yui";
const BIN: &str = "yui";

/// How long [`finalize_auto_update_check`] waits for an in-flight
/// background install before giving up (silently). Keeps fast commands
/// snappy.
const INSTALL_FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`finalize_auto_update_check`] waits for an in-flight notify
/// check before falling back to the cached release.
const NOTIFY_FINALIZE_TIMEOUT: Duration = Duration::from_secs(1);

/// Resolve the transient update-check state file path:
/// `<cache dir>/yui/last_update_check.json`. This is throttle / cache
/// state that can be safely deleted and re-created, so it lives under
/// the OS cache directory (XDG `cache_dir()`), not the persistent data
/// directory that `kaishin::Checker::new` defaults to. Returns `None` if
/// the cache dir can't be resolved (then auto-update is skipped —
/// resilience).
fn state_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join(BIN).join("last_update_check.json"))
}

/// Environment kill-switch for the background auto-update, taking
/// precedence over `[ui] auto_update`. Disabled when
/// `YUI_NO_AUTOUPDATE` is set to anything non-empty other than `"0"`
/// / `"false"` (case-insensitive). The variable name is the
/// uppercased binary name plus `_NO_AUTOUPDATE`, matching renri /
/// rvpm's `<BIN>_NO_AUTOUPDATE` convention.
fn auto_update_disabled_by_env() -> bool {
    match std::env::var(format!("{}_NO_AUTOUPDATE", BIN.to_uppercase())) {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && !v.eq_ignore_ascii_case("0") && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

fn kaishin_opts() -> kaishin::KaishinOptions {
    kaishin::KaishinOptions::new(OWNER, REPO, BIN, env!("CARGO_PKG_VERSION"))
}

fn make_runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// Run `yui self-update`. Flags map directly onto kaishin's
/// `UpdateOptions`:
///
/// - `yes` skips the confirmation prompt.
/// - `check_only` reports availability and exits without installing.
/// - `non_interactive` makes kaishin bail out (rather than prompt)
///   when stdin isn't a tty; only meaningful together with `yes`.
pub fn run_self_update(yes: bool, check_only: bool, non_interactive: bool) -> Result<()> {
    let opts = kaishin_opts();
    let upd_opts = kaishin::UpdateOptions::new()
        .yes(yes)
        .check_only(check_only)
        .non_interactive(non_interactive);

    let rt = make_runtime()?;
    rt.block_on(async { kaishin::run_self_update(&opts, upd_opts).await })
}

/// Default interval between background update checks (24 hours).
pub fn default_interval() -> Duration {
    kaishin::default_interval()
}

/// Thin wrapper over `kaishin::Checker`. The async methods
/// ([`check_and_save`](Self::check_and_save) /
/// [`auto_update`](Self::auto_update)) are meant to be driven as `tokio`
/// tasks spawned on `main`'s runtime so they overlap command execution,
/// rather than on a raw OS worker thread blocked at shutdown. The type
/// is cheaply [`Clone`]able (kaishin's `Checker` is) so it can be moved
/// into a spawned task. Same shape as renri.
#[derive(Clone)]
pub struct Checker {
    inner: kaishin::Checker,
}

impl Checker {
    /// Build a checker pinned to yui's (owner, repo, bin) triple and the
    /// running binary's version. The throttle / cache state file is
    /// pointed at the OS cache dir (`<cache>/yui/last_update_check.json`)
    /// rather than kaishin's default data dir, since it is transient.
    /// Returns `None` if the cache dir can't be resolved.
    pub fn new() -> Option<Self> {
        let path = state_path()?;
        let inner = kaishin::Checker::new(BIN, kaishin_opts()).state_path(path);
        Some(Self { inner })
    }

    /// Override the cadence between background checks. Pair with
    /// `kaishin::parse_interval` when the value comes from the
    /// `[ui] update_check_interval` config string.
    pub fn interval(mut self, interval: Duration) -> Self {
        self.inner = self.inner.interval(interval);
        self
    }

    /// True if the on-disk cache is older than the configured
    /// interval and we should fetch from GitHub again.
    pub fn should_check(&self) -> bool {
        self.inner.should_check()
    }

    /// Hit GitHub, write the result to the cache, and return the
    /// release *only when it actually outranks the running binary*.
    /// `Ok(None)` is "fetched fine, no update"; `Err` is "fetch
    /// failed". Driven on `main`'s tokio runtime as a spawned task.
    pub async fn check_and_save(&self) -> Result<Option<kaishin::LatestRelease>> {
        self.inner.check_and_save().await
    }

    /// Silently download + swap yui's own binary in the background
    /// (the `install` mode). kaishin's `auto_update` is self-throttled
    /// (it checks `should_check` internally), OS-advisory-locked
    /// across concurrent processes, and skips dev builds. It returns
    /// `Ok(Some(rel))` *only when it actually installed* a newer
    /// release, `Ok(None)` when there was nothing to do (throttled,
    /// already latest, lost the lock, or a dev build), and `Err` when
    /// the GitHub fetch / install genuinely failed.
    ///
    /// Driven on `main`'s tokio runtime as a spawned task so it overlaps
    /// command execution.
    pub async fn auto_update(&self) -> Result<Option<kaishin::LatestRelease>> {
        self.inner.auto_update().await
    }

    /// Latest release known from the last successful check; `None`
    /// if no check has ever completed.
    pub fn cached_update(&self) -> Option<kaishin::LatestRelease> {
        self.inner.cached_update()
    }

    /// Render the `A new version is available!` banner kaishin
    /// ships out of the box.
    pub fn format_banner(&self, latest: &kaishin::LatestRelease) -> String {
        self.inner.format_banner(latest)
    }
}

/// The latest-release payload a spawned background task resolves to.
type UpdateResult = Result<Option<kaishin::LatestRelease>>;

/// Handle for an ongoing or cached background update check.
///
/// The in-flight variants carry a [`tokio::task::JoinHandle`] for a task
/// spawned on `main`'s runtime at startup, so the network / IO overlaps
/// the command instead of running on a raw OS worker thread. They are
/// drained with a short, bounded `tokio::time::timeout` in
/// [`finalize_auto_update_check`]. Mirrors renri's `AutoUpdateHandle`.
pub enum AutoUpdateHandle {
    /// A newer version was found in the local cache from a previous
    /// run, and we don't need to hit GitHub again on this invocation.
    CachedAvailable {
        checker: Checker,
        latest: kaishin::LatestRelease,
    },
    /// A background check is running as a spawned task; the join handle
    /// hands the result back at shutdown. `Ok(Ok(None))` means "fetch
    /// succeeded, no update" — distinct from a timeout / error case,
    /// where we may still want to fall back to the cached release.
    Pending {
        checker: Checker,
        handle: tokio::task::JoinHandle<UpdateResult>,
        cached_latest: Option<kaishin::LatestRelease>,
    },
    /// `install` mode: a background `auto_update` is running as a
    /// spawned task. It hands back `Ok(Ok(Some(rel)))` *only when a new
    /// version was actually installed*; everything else (no update, lost
    /// lock, dev build, fetch error, timeout) stays silent.
    Installing {
        handle: tokio::task::JoinHandle<UpdateResult>,
    },
}

/// Spawn the background auto-update work as a `tokio` task on `rt` so it
/// overlaps command execution, honoring the `YUI_NO_AUTOUPDATE`
/// kill-switch and the resolved `[ui] auto_update` mode. The returned
/// handle is consumed by [`finalize_auto_update_check`] at shutdown.
///
/// Source-repo discovery is best-effort: we read `[ui]` config to
/// pick the mode + cadence, but if the repo can't be located we just
/// skip the work rather than fail loudly. Auto-update is convenience;
/// nothing else hangs off of it.
///
/// Modes:
/// - `Off` → do nothing.
/// - `Notify` → the original behavior: check (throttled / cached) and
///   show a banner at exit if a newer release exists, never install.
/// - `Install` → if the throttle window has elapsed, run kaishin's
///   silent background install as a spawned task; print one line at
///   exit only if it actually installed.
pub fn maybe_spawn_auto_update_check(
    rt: &tokio::runtime::Handle,
    cli_source: Option<&Utf8Path>,
) -> Option<AutoUpdateHandle> {
    // Env kill-switch takes precedence over config.
    if auto_update_disabled_by_env() {
        return None;
    }

    let source = detect_source(cli_source)?;
    let yui = YuiVars::detect(&source);
    let loaded = config::load(&source, &yui).ok()?;
    let mode = loaded.ui.update_mode();
    if mode == AutoUpdateMode::Off {
        return None;
    }

    // Surface a malformed `update_check_interval` rather than
    // silently rolling it into the default. A typo here is exactly
    // the sort of thing the user would want to know about; logging
    // through `tracing::warn!` lets `-v` reveal it without crashing
    // the rest of the command. (PR #76 review by coderabbitai.)
    let interval = match loaded.ui.update_check_interval.as_deref() {
        None => default_interval(),
        Some(s) => match kaishin::parse_interval(s) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    "invalid [ui] update_check_interval = {s:?} ({e}); \
                     falling back to default {:?}",
                    default_interval()
                );
                default_interval()
            }
        },
    };

    let checker = Checker::new()?.interval(interval);

    match mode {
        AutoUpdateMode::Off => None,
        AutoUpdateMode::Notify => {
            if !checker.should_check() {
                if let Some(latest) = checker.cached_update() {
                    return Some(AutoUpdateHandle::CachedAvailable { checker, latest });
                }
                return None;
            }

            let cached_latest = checker.cached_update();
            let task_checker = checker.clone();
            let handle = rt.spawn(async move { task_checker.check_and_save().await });

            Some(AutoUpdateHandle::Pending {
                checker,
                handle,
                cached_latest,
            })
        }
        AutoUpdateMode::Install => {
            // `auto_update` is itself self-throttled, but gating on
            // `should_check` here avoids spawning a task (and the
            // OS-lock dance) on every fast command inside the window.
            if !checker.should_check() {
                return None;
            }
            let handle = rt.spawn(async move { checker.auto_update().await });
            Some(AutoUpdateHandle::Installing { handle })
        }
    }
}

/// Finish the background auto-update work before the binary exits.
///
/// Each in-flight task is drained with a short, bounded
/// `tokio::time::timeout` driven on `rt` — this never blocks an OS
/// worker thread synchronously, and a still-running task is simply
/// abandoned at process exit.
///
/// For `notify` mode (`CachedAvailable` / `Pending`) this prints the
/// "new version available" banner, waiting up to one second for an
/// in-flight check; on timeout it falls back to the previously-cached
/// release so a slow GitHub doesn't suppress the nudge. The
/// "fetched, no update" case (`Ok(Ok(None))`) is distinct from a
/// timeout/error and skips the banner entirely, since the cache can't
/// be newer than the fresh fetch we just completed.
///
/// For `install` mode (`Installing`) this waits a *short* bounded
/// window (5s) for the background install and, only if a new version
/// was actually installed, prints exactly one line. Every other
/// outcome (timeout, error, nothing installed) stays silent — fast
/// yui commands must never hang waiting on a slow download.
pub fn finalize_auto_update_check(rt: &tokio::runtime::Handle, handle: AutoUpdateHandle) {
    match handle {
        AutoUpdateHandle::CachedAvailable { checker, latest } => {
            eprintln!("\n{}", checker.format_banner(&latest));
        }
        AutoUpdateHandle::Pending {
            checker,
            handle,
            cached_latest,
        } => {
            let res =
                rt.block_on(async { tokio::time::timeout(NOTIFY_FINALIZE_TIMEOUT, handle).await });
            match res {
                Ok(Ok(Ok(Some(latest)))) => {
                    eprintln!("\n{}", checker.format_banner(&latest));
                }
                Ok(Ok(Ok(None))) => {
                    // Fetched fine, no update — and no cache fallback needed.
                }
                _ => {
                    // Timeout / join error / fetch error: fall back to cache.
                    if let Some(latest) = cached_latest {
                        eprintln!("\n{}", checker.format_banner(&latest));
                    }
                }
            }
        }
        AutoUpdateHandle::Installing { handle } => {
            // SHORT bounded wait: a fast command must not block on a
            // slow download. Print a single line only on a real
            // install; timeout / error / "nothing installed" stay
            // silent (resilience).
            let res =
                rt.block_on(async { tokio::time::timeout(INSTALL_FINALIZE_TIMEOUT, handle).await });
            if let Ok(Ok(Ok(Some(latest)))) = res {
                eprintln!(
                    "\u{2713} {} {} installed in the background — restart to apply.",
                    BIN,
                    latest.tag_name.trim_start_matches('v')
                );
            }
        }
    }
}

/// Best-effort source-repo resolution for the banner path. Honors
/// `--source` / `$YUI_SOURCE` first, then walks cwd ancestors
/// looking for a `config.toml`. Skips the `~/dotfiles` fallback
/// that `cmd::resolve_source` does — the banner shouldn't surprise
/// users running `yui` from outside their dotfiles repo.
fn detect_source(cli_source: Option<&Utf8Path>) -> Option<Utf8PathBuf> {
    if let Some(s) = cli_source {
        return Some(absolutize_best_effort(s));
    }
    if let Ok(s) = std::env::var("YUI_SOURCE") {
        return Some(absolutize_best_effort(Utf8Path::new(&s)));
    }
    let cwd = current_dir()?;
    for ancestor in cwd.ancestors() {
        if ancestor.join("config.toml").is_file() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

fn absolutize_best_effort(p: &Utf8Path) -> Utf8PathBuf {
    let expanded = paths::expand_tilde(p.as_str());
    if expanded.is_absolute() {
        return expanded;
    }
    current_dir()
        .map(|cwd| cwd.join(&expanded))
        .unwrap_or(expanded)
}

fn current_dir() -> Option<Utf8PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    Utf8PathBuf::from_path_buf(cwd).ok()
}
