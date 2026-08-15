use super::*;
use anyhow::{Context as _, Result};
use camino::{Utf8Path, Utf8PathBuf};
use tracing::{info, warn};

pub fn init(source: Option<Utf8PathBuf>, git_hooks: bool) -> Result<()> {
    let dir = match source {
        Some(s) => absolutize(&s)?,
        None => current_dir_utf8()?,
    };
    std::fs::create_dir_all(&dir)?;
    let config_path = dir.join("config.toml");
    let scaffolded = if !config_path.exists() {
        std::fs::write(&config_path, SKELETON_CONFIG)?;
        info!("initialized yui source repo at {dir}");
        info!("created: {config_path}");
        true
    } else if git_hooks {
        // Existing repo + hooks-only invocation: just install the
        // hooks. Don't bail like we used to — a user who already has
        // a populated dotfiles repo shouldn't need to delete
        // config.toml to opt into the render-drift hooks.
        info!(
            "config.toml already exists at {config_path} \
             — skipping scaffold, installing git hooks only"
        );
        false
    } else {
        anyhow::bail!("config.toml already exists at {config_path}");
    };

    // .gitignore upkeep is `init`'s responsibility — running it
    // again on an existing repo (e.g. for a hooks-only install)
    // should still backfill the yui-required ignore lines if the
    // .gitignore has drifted. The rendered-template section is
    // separately maintained by `apply`'s render flow, so we only
    // touch the state / backup / config.local entries here.
    ensure_gitignore_yui_entries(&dir)?;

    if git_hooks {
        install_git_hooks(&dir)?;
    }
    if scaffolded {
        info!("next: edit config.toml, then run `yui apply`");
    }
    Ok(())
}

/// .gitignore lines yui needs every dotfiles repo to carry. Anything
/// the render flow auto-manages (the `# >>> yui rendered ... <<<`
/// section) lives there; what `init` owns is the per-machine state +
/// backup pile + the `config.local.toml` carve-out.
const YUI_REQUIRED_GITIGNORE: &[&str] = &[
    "/.yui/state.json",
    "/.yui/state.json.tmp",
    "/.yui/backup/",
    "config.local.toml",
];

/// Ensure each `YUI_REQUIRED_GITIGNORE` line is present in the repo's
/// `.gitignore`. Creates the file with the full skeleton when it's
/// missing entirely, and appends only the missing entries (in a
/// labelled section) when it already exists. Idempotent — re-running
/// `init` is a no-op once the entries are in place.
fn ensure_gitignore_yui_entries(dir: &Utf8Path) -> Result<()> {
    let path = dir.join(".gitignore");
    if !path.exists() {
        std::fs::write(&path, SKELETON_GITIGNORE)?;
        info!("created: {path}");
        return Ok(());
    }
    let existing = std::fs::read_to_string(&path)?;
    let missing: Vec<&str> = YUI_REQUIRED_GITIGNORE
        .iter()
        .copied()
        .filter(|entry| !existing.lines().any(|line| line.trim() == *entry))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str("# yui per-machine state and backups (added by `yui init`).\n");
    for entry in &missing {
        next.push_str(entry);
        next.push('\n');
    }
    std::fs::write(&path, next)?;
    info!(
        "updated .gitignore: appended {} yui entr{} ({})",
        missing.len(),
        if missing.len() == 1 { "y" } else { "ies" },
        missing.join(", ")
    );
    Ok(())
}

/// Install yui's render-drift hooks into the source repo's
/// `.git/hooks/`. Both pre-commit and pre-push run `yui render --check`
/// — pre-commit catches the easy case (you forgot to `apply` before
/// committing), pre-push is the safety net that catches anything a
/// bypassed pre-commit (or a `git commit --no-verify`) let slip
/// through.
///
/// Asks git for the hooks directory via `rev-parse --git-path hooks`
/// so `core.hooksPath` (configured globally or per-repo to redirect
/// hooks elsewhere) is honoured, and worktrees / bare repos / GIT_DIR
/// overrides come along for the ride. Refuses to overwrite existing
/// hooks — the user has to delete them first if they want yui to
/// manage that slot.
fn install_git_hooks(source: &Utf8Path) -> Result<()> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(source.as_std_path())
        .output()
        .with_context(|| format!("git rev-parse --git-path hooks in {source}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "--git-hooks: {source} doesn't look like a git repo \
             (run `git init` first). git: {}",
            stderr.trim()
        );
    }
    let raw = String::from_utf8(out.stdout)?;
    let hooks_dir = {
        let p = Utf8PathBuf::from(raw.trim());
        if p.is_absolute() { p } else { source.join(p) }
    };
    std::fs::create_dir_all(&hooks_dir).with_context(|| format!("mkdir -p {hooks_dir}"))?;

    for (name, body) in [("pre-commit", PRE_COMMIT_HOOK), ("pre-push", PRE_PUSH_HOOK)] {
        let path = hooks_dir.join(name);
        if path.exists() {
            warn!("--git-hooks: {path} already exists — leaving it alone");
            continue;
        }
        std::fs::write(&path, body).with_context(|| format!("write hook {path}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms)?;
        }
        info!("installed: {path}");
    }
    Ok(())
}

const PRE_COMMIT_HOOK: &str = r#"#!/bin/sh
# Installed by `yui init --git-hooks`.
# Reject the commit if any `*.tera` template would render to something
# that diverges from the rendered output staged alongside it. Run
# `yui apply` (or `yui render`) to refresh and re-commit.
exec yui render --check
"#;

const PRE_PUSH_HOOK: &str = r#"#!/bin/sh
# Installed by `yui init --git-hooks`.
# Same render-drift check as pre-commit, mirrored on push so a
# `--no-verify` commit doesn't sneak diverged state to the remote.
exec yui render --check
"#;

const SKELETON_CONFIG: &str = r#"# yui config — see https://github.com/yukimemi/yui

[vars]
# user-defined values; templates can reference these as {{ vars.foo }}

[mount]
default_strategy = "marker"
# How links are made. `auto` = symlink on Unix, hardlink (files) +
# junction (dirs) on Windows, which needs no Developer Mode / admin.
# file_mode = "auto"   # auto | symlink | hardlink
# dir_mode  = "auto"   # auto | symlink | junction

[[mount.entry]]
src = "home"
# `~` expands to $HOME / $USERPROFILE per OS at apply time, no Tera needed.
dst = "~"

# [[mount.entry]]
# src  = "appdata"
# dst  = "{{ env(name='APPDATA') }}"
# # NOTE: write `when` as a *bare* expression (no `{{ … }}`) so it survives
# # config.toml's whole-file Tera render and shows up cleanly in `yui list`.
# when = "yui.os == 'windows'"

# Explicit links, declared centrally instead of with a `.yuilink`
# marker file in the tree. `src` is relative to $DOTFILES and may name
# a directory (linked as one unit) or a single file.
# [[link]]
# src = "home/.config/nvim"
# dst = "{{ env(name='LOCALAPPDATA') }}/nvim"
# when = "yui.os == 'windows'"
"#;

const SKELETON_GITIGNORE: &str = r#"# yui per-machine state and backups (regenerable, do not commit).
# .yui/bin/ is intentionally tracked — it holds your hook scripts.
/.yui/state.json
/.yui/state.json.tmp
/.yui/backup/

# >>> yui rendered (auto-managed, do not edit) >>>
# <<< yui rendered (auto-managed) <<<

# config.local.toml is per-machine; commit a config.local.example.toml instead.
config.local.toml
"#;
