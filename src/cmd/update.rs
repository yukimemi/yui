use super::*;
use anyhow::Result;
use camino::Utf8PathBuf;
use tracing::info;

/// `yui update [--dry-run]` — pull source repo and re-apply.
///
/// Equivalent to `git -C $DOTFILES pull --ff-only && yui apply`,
/// but with the safety check that the source tree is clean first
/// (otherwise the pull could mix upstream commits with the user's
/// in-progress edits in surprising ways). Bails on a dirty source
/// rather than stashing — the user should commit consciously.
///
/// `--dry-run` only forwards to `apply --dry-run`; the pull itself
/// always runs (it's a read+merge operation, no half-state).
pub fn update(source: Option<Utf8PathBuf>, dry_run: bool) -> Result<()> {
    let source = resolve_source(source)?;
    if !crate::git::is_clean(&source)? {
        anyhow::bail!(
            "source repo {source} has uncommitted changes — \
             commit or stash before `yui update` (or run \
             `git pull` + `yui apply` manually if you know what \
             you're doing)"
        );
    }
    info!("git pull --ff-only at {source}");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(source.as_str())
        .arg("pull")
        .arg("--ff-only")
        .status()
        .map_err(|e| anyhow::anyhow!("invoking git: {e}"))?;
    if !status.success() {
        anyhow::bail!("git pull --ff-only failed at {source}");
    }
    apply(Some(source), dry_run)
}
