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
    let update_check_handle = if banner_eligible {
        updater::maybe_spawn_auto_update_check(cli.source.as_deref())
    } else {
        None
    };

    let result = cli.run();

    if let Some(handle) = update_check_handle {
        updater::finalize_auto_update_check(handle);
    }

    result
}
