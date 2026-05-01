use anyhow::Result;
use camino::Utf8PathBuf;
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

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

    /// Manually absorb a target into source (when auto-absorb skipped).
    ///
    /// Prints a unified diff (source vs target) on stderr. Without
    /// `--yes`, prompts on a TTY before writing; off a TTY refuses
    /// to act unless `--yes` is given. `--dry-run` only shows the
    /// diff and exits.
    Absorb {
        target: Utf8PathBuf,
        #[arg(long)]
        dry_run: bool,
        /// Skip the y/N prompt (still prints the diff).
        #[arg(long)]
        yes: bool,
    },

    /// Diagnose environment (symlink capability, source detection, etc)
    Doctor {
        /// Override [ui] icons mode for this invocation
        #[arg(long, value_name = "MODE")]
        icons: Option<IconsMode>,
        /// Disable color output (also respected via NO_COLOR env)
        #[arg(long)]
        no_color: bool,
    },

    /// Garbage-collect old backups under `$DOTFILES/.yui/backup/`.
    ///
    /// With no `--older-than`, prints every parsed backup with its
    /// age + size and exits without deleting (a survey).
    /// With `--older-than DUR`, deletes entries whose timestamp
    /// suffix is older than DUR. Backups whose name doesn't match
    /// yui's `<stem>_<YYYYMMDD_HHMMSSfff>[.<ext>]` shape are left
    /// alone — anything you dropped into `.yui/backup/` by hand
    /// stays there.
    GcBackup {
        /// Age threshold; e.g. `30d`, `2w`, `12h`, `6mo`, `1y`.
        /// Omit to run a non-destructive survey instead.
        #[arg(long, value_name = "DUR")]
        older_than: Option<String>,
        /// Preview the deletion (no files removed). Only meaningful
        /// when `--older-than` is also given.
        #[arg(long)]
        dry_run: bool,
        /// Override [ui] icons mode for this invocation
        #[arg(long, value_name = "MODE")]
        icons: Option<IconsMode>,
        /// Disable color output (also respected via NO_COLOR env)
        #[arg(long)]
        no_color: bool,
    },

    /// Manage `[[hook]]` scripts
    Hooks {
        #[command(subcommand)]
        action: HookAction,
    },

    /// Pull source repo and re-apply (`git pull --ff-only` + `apply`).
    ///
    /// Refuses to run with a dirty source tree — pulling on top of
    /// uncommitted changes mixes upstream work with the user's
    /// in-progress edits in ways that are easy to get wrong. Commit
    /// (or stash) first.
    Update {
        /// Render templates / link targets in dry-run after the pull.
        #[arg(long)]
        dry_run: bool,
    },

    /// List source files NOT claimed by any `[[mount.entry]]` — yui's
    /// "what's just sitting in the repo unused?" report. Skips
    /// `.yui/`, `.git/`, anything matched by `.yuiignore`, and the
    /// repo's own meta files (`config*.toml`, `.yuilink`, `.gitignore`,
    /// `.yuiignore`, `*.tera` template sources).
    Unmanaged {
        /// Override [ui] icons mode for this invocation
        #[arg(long, value_name = "MODE")]
        icons: Option<IconsMode>,
        /// Disable color output (also respected via NO_COLOR env)
        #[arg(long)]
        no_color: bool,
    },

    /// Print a unified diff for every entry that's drifted from
    /// source — like `status` but with full content. Render-drift
    /// rows show the rendered file vs what the template would
    /// produce now; link-drift rows show source vs target.
    Diff {
        /// Override [ui] icons mode for this invocation
        #[arg(long, value_name = "MODE")]
        icons: Option<IconsMode>,
        /// Disable color output (also respected via NO_COLOR env)
        #[arg(long)]
        no_color: bool,
    },

    /// Manage secret files (`*.age`, encrypted with age).
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },

    /// Generate shell completion script for `<shell>` to stdout.
    ///
    /// Pipe into the right place for your shell, e.g.
    /// `yui completion bash > ~/.local/share/bash-completion/completions/yui`,
    /// `yui completion zsh   > "${fpath[1]}/_yui"`,
    /// `yui completion pwsh  | Out-String | Invoke-Expression`.
    Completion {
        /// Target shell (bash / zsh / fish / powershell / elvish).
        shell: Shell,
    },
}

#[derive(Subcommand, Debug)]
pub enum SecretAction {
    /// Generate an X25519 keypair, write the secret to
    /// `[secrets] identity` (`~/.config/yui/age.txt` by default),
    /// and append the public key to `[secrets] recipients` in
    /// `$DOTFILES/config.local.toml`. Idempotent — refuses to
    /// overwrite an existing identity file.
    Init {
        /// Append a comment block above the new recipient entry
        /// (defaults to "<host> <user>" — yui.host / yui.user).
        #[arg(long, value_name = "TEXT")]
        comment: Option<String>,
    },
    /// Read `<path>` (absolute or relative to `$DOTFILES`) as
    /// plaintext, encrypt it to every recipient in
    /// `[secrets] recipients`, and write the ciphertext as
    /// `<path>.age` next to it. Refuses to clobber an existing
    /// `.age` without `--force`.
    Encrypt {
        path: Utf8PathBuf,
        /// Replace an existing `<path>.age`.
        #[arg(long)]
        force: bool,
        /// Delete the plaintext after a successful encryption
        /// (only works when the plaintext lives under `$DOTFILES`).
        #[arg(long)]
        rm_plaintext: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum HookAction {
    /// List configured hooks with their last-run state
    List {
        /// Override [ui] icons mode for this invocation
        #[arg(long, value_name = "MODE")]
        icons: Option<IconsMode>,
        /// Disable color output (also respected via NO_COLOR env)
        #[arg(long)]
        no_color: bool,
    },
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
            Command::Absorb {
                target,
                dry_run,
                yes,
            } => cmd::absorb(source, target, dry_run, yes),
            Command::Doctor { icons, no_color } => cmd::doctor(source, icons, no_color),
            Command::GcBackup {
                older_than,
                dry_run,
                icons,
                no_color,
            } => cmd::gc_backup(source, older_than, dry_run, icons, no_color),
            Command::Hooks { action } => match action {
                HookAction::List { icons, no_color } => cmd::hooks_list(source, icons, no_color),
                HookAction::Run { name, force } => cmd::hooks_run(source, name, force),
            },
            Command::Update { dry_run } => cmd::update(source, dry_run),
            Command::Unmanaged { icons, no_color } => cmd::unmanaged(source, icons, no_color),
            Command::Diff { icons, no_color } => cmd::diff(source, icons, no_color),
            Command::Secret { action } => match action {
                SecretAction::Init { comment } => cmd::secret_init(source, comment),
                SecretAction::Encrypt {
                    path,
                    force,
                    rm_plaintext,
                } => cmd::secret_encrypt(source, path, force, rm_plaintext),
            },
            Command::Completion { shell } => {
                let mut cmd = Cli::command();
                clap_complete::generate(shell, &mut cmd, "yui", &mut std::io::stdout());
                Ok(())
            }
        }
    }
}
