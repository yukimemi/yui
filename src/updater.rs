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
//! Auto-update banner / background `Checker` plumbing is
//! intentionally not wired up here — that lives in a follow-up so
//! this change stays scoped to "add the command".

use anyhow::Result;

const OWNER: &str = "yukimemi";
const REPO: &str = "yui";
const BIN: &str = "yui";

/// Run `yui self-update`. Flags map directly onto kaishin's
/// `UpdateOptions`:
///
/// - `yes` skips the confirmation prompt.
/// - `check_only` reports availability and exits without installing.
/// - `non_interactive` makes kaishin bail out (rather than prompt)
///   when stdin isn't a tty; only meaningful together with `yes`.
pub fn run_self_update(yes: bool, check_only: bool, non_interactive: bool) -> Result<()> {
    let opts = kaishin::KaishinOptions::new(OWNER, REPO, BIN, env!("CARGO_PKG_VERSION"));
    let upd_opts = kaishin::UpdateOptions::new()
        .yes(yes)
        .check_only(check_only)
        .non_interactive(non_interactive);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async { kaishin::run_self_update(&opts, upd_opts).await })
}
