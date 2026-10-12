//! Clone-first initialization, without altering the fetched configuration.
use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::process::Command;
use tracing::{info, warn};

use super::common::absolutize;

struct RepoSpec {
    url: String,
}

impl RepoSpec {
    fn parse(input: &str) -> Result<Self> {
        if input.is_empty()
            || input.starts_with('-')
            || input.chars().any(char::is_whitespace)
            || input.chars().any(char::is_control)
            || input.contains('\\')
        {
            bail!("invalid repository; use owner/repo or an explicit Git URL");
        }
        let parts: Vec<_> = input.split('/').collect();
        let name = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        };
        if parts.len() == 2
            && !parts[0].is_empty()
            && !parts[0].starts_with('-')
            && !parts[0].ends_with('-')
            && parts[0]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && name(parts[1].strip_suffix(".git").unwrap_or(parts[1]))
        {
            return Ok(Self {
                url: format!(
                    "https://github.com/{}.git",
                    input.strip_suffix(".git").unwrap_or(input)
                ),
            });
        }
        let url = ["https://", "http://", "ssh://", "git://", "file://"]
            .iter()
            .any(|prefix| {
                input
                    .strip_prefix(prefix)
                    .is_some_and(|tail| !tail.is_empty())
            });
        let scp = input.split_once(':').is_some_and(|(host, path)| {
            (host
                .split_once('@')
                .is_some_and(|(user, host)| name(user) && name(host))
                || (name(host) && (host.contains('.') || host == "localhost")))
                && !path.is_empty()
        });
        if !url && !scp {
            bail!(
                "invalid repository {input:?}; use owner/repo, a Git URL, or user@host:path (local paths are not supported; use file://)"
            );
        }
        Ok(Self {
            url: input.to_owned(),
        })
    }

    fn identity(url: &str) -> String {
        let github = [
            "https://github.com/",
            "http://github.com/",
            "ssh://git@github.com/",
            "git@github.com:",
        ]
        .iter()
        .find_map(|prefix| url.strip_prefix(prefix));
        if let Some(path) = github {
            let path = path.trim_end_matches('/');
            let path = path.strip_suffix(".git").unwrap_or(path);
            let parts: Vec<_> = path.split('/').collect();
            if parts.len() == 2 && parts.iter().all(|p| !p.is_empty()) {
                return format!("github:{}", path.to_ascii_lowercase());
            }
        }
        url.to_owned()
    }
}

fn git() -> Command {
    let mut cmd = Command::new("git");
    // Repository selection must come from the destination, not inherited overrides.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        cmd.env_remove(key);
    }
    cmd
}

fn git_output(dir: &Utf8Path, args: &[&str]) -> Result<String> {
    let out = git()
        .current_dir(dir)
        .args(args)
        .output()
        .context("running git; install Git and ensure it is on PATH")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

fn reuse(dir: &Utf8Path, repo: &RepoSpec) -> Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!(
            "destination {dir} already exists and is not a repository directory; choose another --source"
        );
    }
    let root = git_output(dir, &["rev-parse", "--show-toplevel"])
        .with_context(|| format!("destination {dir} already exists; only the same repository's working tree root can be reused; choose another --source"))?;
    if std::fs::canonicalize(&root)? != std::fs::canonicalize(dir)? {
        bail!(
            "destination {dir} is inside another repository, not its root; choose another --source"
        );
    }
    let origin = git_output(dir, &["remote", "get-url", "origin"])
        .context("existing destination has no usable origin; choose another --source")?;
    if RepoSpec::identity(&origin) != RepoSpec::identity(&repo.url) {
        bail!(
            "destination {dir} belongs to a different repository (origin does not match); choose another --source"
        );
    }
    if !crate::git::is_clean(dir)? {
        warn!("{dir} has local changes; preserving them without fetching, pulling, or resetting");
    }
    info!("reusing existing repository at {dir}; no fetch or pull performed");
    Ok(())
}

fn clone_repo(dir: &Utf8Path, repo: &RepoSpec) -> Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => return reuse(dir, repo),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let parent = dir
        .parent()
        .context("destination must have a parent directory")?;
    std::fs::create_dir_all(parent)?;
    let mut attempt = 0_u64;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let stage = loop {
        let candidate = parent.join(format!(
            ".yui-clone-{}-{stamp}-{attempt}",
            std::process::id()
        ));
        match std::fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => attempt += 1,
            Err(e) => return Err(e.into()),
        }
    };
    let cloned = (|| -> Result<()> {
        let out = git()
            .args(["clone", "--"])
            .arg(&repo.url)
            .arg(&stage)
            .output()
            .context("running git clone; install Git and ensure it is on PATH")?;
        if !out.status.success() {
            bail!(
                "git clone failed: {}. Check the repository URL, network, and Git credentials; apply was not run",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    })();
    if let Err(e) = cloned {
        if let Err(cleanup) = std::fs::remove_dir_all(&stage) {
            warn!("could not remove failed clone at {stage}: {cleanup}");
        }
        return Err(e);
    }
    crate::link::rename_dir_noreplace(&stage, dir)
        .with_context(|| format!("cannot publish clone to {dir}; destination was not replaced. Retrieved repository is preserved at {stage}; move it to an unused --source destination"))?;
    info!("cloned source repository to {dir}");
    Ok(())
}

pub fn bootstrap(
    source: Option<Utf8PathBuf>,
    repo: &str,
    git_hooks: bool,
    apply: bool,
) -> Result<()> {
    let repo = RepoSpec::parse(repo)?;
    let dir = match source {
        Some(source) => absolutize(&source)?,
        None => crate::paths::home_dir()
            .context("home directory unavailable; specify --source")?
            .join("dotfiles"),
    };
    clone_repo(&dir, &repo)?;
    if git_hooks {
        super::init::install_git_hooks(&dir)?;
    }
    if !dir.join("config.toml").is_file() {
        if apply {
            bail!(
                "repository initialized at {dir}, but config.toml is missing; add the configuration, then run `yui --source {dir} apply`"
            );
        }
        warn!("config.toml is missing at {dir}; add the configuration before running yui apply");
    }
    if apply {
        super::apply(Some(dir.clone()), false).with_context(|| format!("repository remains initialized at {dir}; resolve the apply error, then run `yui --source {dir} apply`"))?;
    } else {
        info!(
            "next: cd {dir}, unlock secrets if needed with `yui secret unlock`, then run `yui apply`"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_arguments_and_identity() {
        assert_eq!(
            RepoSpec::parse("owner/repo").unwrap().url,
            "https://github.com/owner/repo.git"
        );
        assert_eq!(
            RepoSpec::parse("owner/repo.git").unwrap().url,
            "https://github.com/owner/repo.git"
        );
        for url in [
            "https://example.com/repo.git",
            "ssh://git@example.com/repo",
            "git@example.com:repo",
            "example.com:repo",
            "file:///tmp/repo",
        ] {
            assert_eq!(RepoSpec::parse(url).unwrap().url, url);
        }
        for invalid in [
            "",
            "-option",
            "../repo",
            "./repo",
            "/tmp/repo",
            "C:/repo",
            "C:\\repo",
            "owner/repo/extra",
            "owner.name/repo",
            "owner/.git",
            "owner/repo\n",
            "https://",
        ] {
            assert!(RepoSpec::parse(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(
            RepoSpec::identity("https://github.com/Owner/Repo.git"),
            RepoSpec::identity("git@github.com:owner/repo")
        );
        assert_eq!(
            RepoSpec::identity("ssh://git@github.com/owner/repo.git"),
            RepoSpec::identity("http://github.com/owner/repo/")
        );
        assert_ne!(
            RepoSpec::identity("file:///a"),
            RepoSpec::identity("file:///b")
        );
    }
}
