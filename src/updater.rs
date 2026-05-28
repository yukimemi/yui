//! Self-update support for yui, using the shared `kaishin` library.
//!
//! Thin sync facade around kaishin's async API so the rest of yui
//! can stay synchronous. Same shape as renri's `src/updater.rs`;
//! the only yui-specific bit is the hardcoded `(owner, repo, bin)`
//! triple — yui's crate name is `yui-cli` (because crates.io's
//! `yui` is taken by an unrelated abandoned crate), but the repo
//! and binary are both `yui`, so going through `env!("CARGO_PKG_NAME")`
//! the way renri does would produce the wrong GitHub Release URL.
//!
//! The module exposes two layers:
//!
//! - [`run_self_update`] — drives the `yui self-update` subcommand
//!   (interactive / `--yes` / `--check`).
//! - [`Checker`] + [`maybe_spawn_auto_update_check`] /
//!   [`finalize_auto_update_check`] — the daily background banner
//!   shown after every other subcommand, the way `rvpm` / `renri`
//!   do it. `[ui] auto_update_check = false` in `config.toml` opts
//!   out, and `[ui] update_check_interval = "..."` overrides the
//!   default 24h cadence.

use std::time::Duration;

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

use crate::config;
use crate::paths;
use crate::vars::YuiVars;

const OWNER: &str = "yukimemi";
const REPO: &str = "yui";
const BIN: &str = "yui";

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

/// Blocking facade over `kaishin::Checker` — yui's main loop is
/// synchronous, so the async `check_and_save` call goes through a
/// fresh `current_thread` runtime each time. Same shape as renri.
pub struct Checker {
    inner: kaishin::Checker,
}

impl Checker {
    /// Build a checker pinned to yui's (owner, repo, bin) triple
    /// and the running binary's version.
    pub fn new() -> Result<Self> {
        let inner = kaishin::Checker::new(BIN, kaishin_opts());
        Ok(Self { inner })
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
    /// failed". Block on an ad-hoc tokio runtime — the caller is on
    /// a regular OS thread spawned by `std::thread::spawn`.
    pub fn check_and_save(&self) -> Result<Option<kaishin::LatestRelease>> {
        let rt = make_runtime()?;
        rt.block_on(async { self.inner.check_and_save().await })
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

/// Handle for an ongoing or cached background update check.
/// Mirrors renri's `AutoUpdateHandle`.
pub enum AutoUpdateHandle {
    /// A newer version was found in the local cache from a previous
    /// run, and we don't need to hit GitHub again on this invocation.
    CachedAvailable {
        checker: Checker,
        latest: kaishin::LatestRelease,
    },
    /// A background check is running on a worker thread; the receiver
    /// hands the result back to the main thread at shutdown.
    /// `Ok(Ok(None))` means "fetch succeeded, no update" — distinct
    /// from a timeout/error case, where we may still want to fall back
    /// to the cached release.
    Pending {
        checker: Checker,
        rx: std::sync::mpsc::Receiver<Result<Option<kaishin::LatestRelease>>>,
        cached_latest: Option<kaishin::LatestRelease>,
    },
}

/// Spawn a background check on `std::thread::spawn` if the user
/// hasn't opted out and the cache is older than the configured
/// interval. The returned handle is consumed by
/// [`finalize_auto_update_check`] at shutdown.
///
/// Source-repo discovery is best-effort: we read `[ui]` config to
/// see whether the banner is disabled, but if the repo can't be
/// located we just skip the banner rather than fail loudly. The
/// banner is convenience; nothing else hangs off of it.
pub fn maybe_spawn_auto_update_check(cli_source: Option<&Utf8Path>) -> Option<AutoUpdateHandle> {
    let source = detect_source(cli_source)?;
    let yui = YuiVars::detect(&source);
    let loaded = config::load(&source, &yui).ok()?;
    if !loaded.ui.auto_update_check {
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

    let checker = Checker::new().ok()?.interval(interval);

    if !checker.should_check() {
        if let Some(latest) = checker.cached_update() {
            return Some(AutoUpdateHandle::CachedAvailable { checker, latest });
        }
        return None;
    }

    let cached_latest = checker.cached_update();
    let (tx, rx) = std::sync::mpsc::channel();
    let checker_clone = Checker::new().ok()?.interval(interval);
    std::thread::spawn(move || {
        let _ = tx.send(checker_clone.check_and_save());
    });

    Some(AutoUpdateHandle::Pending {
        checker,
        rx,
        cached_latest,
    })
}

/// Print the update banner (if any) before the binary exits. Waits
/// up to one second for an in-flight background check to finish; on
/// timeout, falls back to the previously-cached release so the user
/// still gets the nudge.
///
/// kaishin 0.4 made `check_and_save` return `Result<Option<_>>`, so
/// the "fetched, no update" case (`Ok(Ok(None))`) is now distinct
/// from a timeout/error: in that case we skip the banner entirely
/// instead of falling back to cache, since the cache can't be newer
/// than the fresh fetch we just completed. Timeout/error still falls
/// back to cache so a slow GitHub doesn't suppress the nudge.
pub fn finalize_auto_update_check(handle: AutoUpdateHandle) {
    let (checker, latest) = match handle {
        AutoUpdateHandle::CachedAvailable { checker, latest } => (checker, Some(latest)),
        AutoUpdateHandle::Pending {
            checker,
            rx,
            cached_latest,
        } => {
            let latest = match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(Some(latest))) => Some(latest),
                Ok(Ok(None)) => None,
                _ => cached_latest,
            };
            (checker, latest)
        }
    };
    if let Some(latest) = latest {
        eprintln!("\n{}", checker.format_banner(&latest));
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
