//! Lightweight git status check via shell-out.
//!
//! `apply` uses this to decide whether to escalate `AutoAbsorb` to
//! `NeedsConfirm` when `[absorb] require_clean_git = true` — pulling
//! target-side edits into a dirty source repo would mix yui's writes
//! with the user's in-progress changes in a single commit.
//!
//! No git crate dependency: shells out to the `git` CLI. If `git` isn't
//! installed or the directory isn't a repo, callers receive a clear
//! `Error::Git` and can decide how lenient to be.

use std::process::Command;

use camino::Utf8Path;

use crate::{Error, Result};

/// Returns `true` when the working tree at `repo` has no uncommitted
/// changes and no untracked files (i.e. `git status --porcelain` is
/// empty). `Err(Error::Git(_))` if `git` isn't on `$PATH` or `repo`
/// isn't a repository.
pub fn is_clean(repo: &Utf8Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo.as_str())
        .arg("status")
        .arg("--porcelain")
        .output()
        .map_err(|e| Error::Git(format!("invoking `git`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!(
            "git status failed at {repo}: {}",
            stderr.trim()
        )));
    }
    Ok(output.stdout.is_empty())
}
