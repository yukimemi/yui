use anyhow::Result;
use camino::Utf8PathBuf;
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand};

use crate::cmd;
use crate::config::IconsMode;

/// Explicit colour palette for `--help` output. clap honours `NO_COLOR`
/// and falls back to monochrome when stdout isn't a TTY, so this is
/// safe to leave on unconditionally — the styled bytes are only ever
/// emitted when a real terminal is reading them. The palette mirrors
/// the bind-points / icon colours used in `yui list` / `yui status` so
/// help, list, and status all feel like the same tool.
const HELP_STYLES: Styles = Styles::styled()
    // "Commands:" / "Options:" / etc. section headers.
    .header(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
    // The "Usage:" heading label itself (NOT the binary name — that
    // falls under `literal` below).
    .usage(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
    // Binary name in the usage line + every subcommand / option
    // literal (`init`, `--source`, …).
    .literal(AnsiColor::Magenta.on_default().effects(Effects::BOLD))
    // <PLACEHOLDER> values inside option signatures.
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD));

#[derive(Parser, Debug)]
#[command(version, about, long_about = None, styles = HELP_STYLES)]
pub struct Cli {
    /// Path to dotfiles source repository ($DOTFILES)
    #[arg(short, long, env = "YUI_SOURCE", global = true)]
    pub source: Option<Utf8PathBuf>,

    /// Increase log verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialize source repo skeleton
    Init {
        /// Install git pre-commit/pre-push hooks for render-drift check
        #[arg(long)]
        git_hooks: bool,
    },

    /// Render templates + link targets + auto-absorb (default workflow)
    Apply {
        #[arg(long)]
        dry_run: bool,
    },

    /// Render templates only
    Render {
        /// Fail with non-zero exit if rendered output diverges (CI hook)
        #[arg(long)]
        check: bool,
        #[arg(long)]
        dry_run: bool,
    },

    /// Link / relink targets only
    Link {
        #[arg(long)]
        dry_run: bool,
    },

    /// Unlink targets
    Unlink { paths: Vec<Utf8PathBuf> },

    /// Show drift status (link-broken / replaced / template-drift)
    Status {
        /// Override [ui] icons mode for this invocation
        #[arg(long, value_name = "MODE")]
        icons: Option<IconsMode>,
        /// Disable color output (also respected via NO_COLOR env)
        #[arg(long)]
        no_color: bool,
    },

    /// List all src→dst link mappings (mount entries + .yuilink overrides)
    List {
        /// Include entries whose `when` evaluates false on the current host
        #[arg(long)]
        all: bool,
        /// Override [ui] icons mode for this invocation
        #[arg(long, value_name = "MODE")]
        icons: Option<IconsMode>,
        /// Disable color output (also respected via NO_COLOR env)
        #[arg(long)]
        no_color: bool,
    },

    /// Manually absorb a target into source (when auto-absorb skipped)
    Absorb {
        target: Utf8PathBuf,
        #[arg(long)]
        dry_run: bool,
    },

    /// Diagnose environment (symlink capability, source detection, etc)
    Doctor,

    /// Garbage-collect old backups
    GcBackup {
        /// e.g. "30d", "6m"
        #[arg(long)]
        older_than: Option<String>,
    },

    /// Manage `[[hook]]` scripts
    Hooks {
        #[command(subcommand)]
        action: HookAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum HookAction {
    /// List configured hooks with their last-run state
    List,
    /// Run a hook (or every hook). The `when` filter is always honored;
    /// `--force` bypasses the `when_run` state check (so a `once` hook
    /// can be re-run, an `onchange` hook re-runs even with matching
    /// hash).
    Run {
        /// Hook name (omit to run every hook per its `when_run` rule)
        name: Option<String>,
        /// Bypass the `when_run` state check
        #[arg(long)]
        force: bool,
    },
}

impl Cli {
    pub fn run(self) -> Result<()> {
        let source = self.source;
        match self.command {
            Command::Init { git_hooks } => cmd::init(source, git_hooks),
            Command::Apply { dry_run } => cmd::apply(source, dry_run),
            Command::Render { check, dry_run } => cmd::render(source, check, dry_run),
            Command::Link { dry_run } => cmd::link(source, dry_run),
            Command::Unlink { paths } => cmd::unlink(source, paths),
            Command::Status { icons, no_color } => cmd::status(source, icons, no_color),
            Command::List {
                all,
                icons,
                no_color,
            } => cmd::list(source, all, icons, no_color),
            Command::Absorb { target, dry_run } => cmd::absorb(source, target, dry_run),
            Command::Doctor => cmd::doctor(source),
            Command::GcBackup { older_than } => cmd::gc_backup(source, older_than),
            Command::Hooks { action } => match action {
                HookAction::List => cmd::hooks_list(source),
                HookAction::Run { name, force } => cmd::hooks_run(source, name, force),
            },
        }
    }
}
