use anyhow::Result;
use clap::Parser;
use yui::cli::{Cli, Command};
use yui::updater;

fn main() -> Result<()> {
    let cli = Cli::parse();
    yui::init_tracing(cli.verbose);

    // Background update check: same pattern as rvpm / renri. Skip on
    // `self-update` (would race with the install) and `completion`
    // (its output is meant to be piped into shell init, so the
    // banner on stderr would surprise / pollute setups that capture
    // stderr too).
    let banner_eligible = !matches!(
        cli.command,
        Command::SelfUpdate { .. } | Command::Completion { .. }
    );

    // A small multi-threaded tokio runtime drives the background
    // auto-update as a spawned task that overlaps the command (mirrors
    // renri / rvpm). One worker is enough: the task is mostly network /
    // IO-bound and is drained with a short, bounded timeout at shutdown.
    // Built lazily so commands that never spawn an update (self-update /
    // completions, or a disabled config) don't pay for it; `None` means
    // we never needed a runtime.
    let mut update_rt: Option<tokio::runtime::Runtime> = None;
    let update_check_handle = if banner_eligible {
        match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        {
            Ok(rt) => {
                let handle =
                    updater::maybe_spawn_auto_update_check(rt.handle(), cli.source.as_deref());
                if handle.is_some() {
                    update_rt = Some(rt);
                }
                handle
            }
            // If even the runtime fails to build, silently skip auto-update.
            Err(_) => None,
        }
    } else {
        None
    };

    let result = cli.run();

    if let (Some(rt), Some(handle)) = (update_rt.as_ref(), update_check_handle) {
        updater::finalize_auto_update_check(rt.handle(), handle);
    }

    result
}
