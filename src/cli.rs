use anyhow::Result;
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};

use crate::cmd;
use crate::config::IconsMode;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
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
        }
    }
}
