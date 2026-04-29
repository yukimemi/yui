//! Path utilities for backup-mirroring, timestamp suffixing, and
//! cross-platform tilde expansion.

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};

/// Expand a leading `~` or `~/...` to the user's home directory.
///
/// Smooths over the `$HOME` (Unix) vs `$USERPROFILE` (Windows) split so
/// `dst = "~/.config"` works on every platform without writing a Tera
/// `env(...)` call. Home is resolved via [`home_dir`].
///
/// `~user` (other-user homes) is left untouched — we don't support that
/// form. If `$HOME` / `$USERPROFILE` are both unset the input is also
/// returned verbatim (better to surface a "no such path" error later than
/// silently substitute an empty string).
pub fn expand_tilde(s: &str) -> Utf8PathBuf {
    match home_dir() {
        Some(home) => expand_tilde_with(s, &home),
        None => Utf8PathBuf::from(s),
    }
}

/// Same as [`expand_tilde`] but with an explicit home path — used in tests
/// to avoid touching the process-wide `HOME` env var.
pub fn expand_tilde_with(s: &str, home: &Utf8Path) -> Utf8PathBuf {
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        home.join(rest)
    } else if s == "~" {
        home.to_path_buf()
    } else {
        Utf8PathBuf::from(s)
    }
}

/// `$HOME` (Unix) or `$USERPROFILE` (Windows), or `None` if neither is set.
pub fn home_dir() -> Option<Utf8PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(Utf8PathBuf::from)
}

/// Load `$source/.yuiignore` as a gitignore-style matcher.
///
/// Returns an empty matcher when the file is absent (so `is_ignored`
/// becomes a no-op). Patterns use full gitignore syntax: glob (`*`,
/// `**`), negation (`!`), trailing-slash dir-only matching, comments
/// (`#`).
///
/// Currently only the repo-root `.yuiignore` is honored — nested
/// `.yuiignore` files inside subdirectories are not yet walked. (The
/// 95% case is "exclude `**/lock.json` once at the top".) If you need
/// per-subtree rules, file an issue with the use case.
pub fn load_yuiignore(source: &Utf8Path) -> crate::Result<ignore::gitignore::Gitignore> {
    let path = source.join(".yuiignore");
    if !path.is_file() {
        return Ok(ignore::gitignore::Gitignore::empty());
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(source);
    if let Some(e) = builder.add(path.as_std_path()) {
        return Err(crate::Error::Config(format!("parsing {path}: {e}")));
    }
    builder
        .build()
        .map_err(|e| crate::Error::Config(format!("building .yuiignore: {e}")))
}

/// Test a path against the loaded `.yuiignore` matcher.
///
/// `path` is treated relative to `source` (gitignore convention). Paths
/// that don't live under `source` can't possibly match a source-rooted
/// rule, so they short-circuit to `false`. Without this guard, an
/// absolute path passed through `unwrap_or(path)` would land on the
/// matcher as an absolute, which `Gitignore` would test using its
/// rightmost component — leading to spurious matches for paths outside
/// the repo. (Caught in PR #19 review.)
///
/// Uses `matched_path_or_any_parents` so an ignored ancestor directory
/// causes the descendant file to be ignored too.
pub fn is_ignored(
    gi: &ignore::gitignore::Gitignore,
    source: &Utf8Path,
    path: &Utf8Path,
    is_dir: bool,
) -> bool {
    let Ok(rel) = path.strip_prefix(source) else {
        return false;
    };
    matches!(
        gi.matched_path_or_any_parents(rel.as_std_path(), is_dir),
        ignore::Match::Ignore(_)
    )
}

/// Build a source-tree walker that skips yui's internal `.yui/` directory.
///
/// `.yui/backup/` can grow huge over time, and `.yui/rendered/` (future)
/// would also live here — neither is part of the user's dotfiles, and
/// walking them slows render / list / absorb-find by a lot. We also keep
/// `.gitignore` / `.ignore` filtering disabled (`git_ignore(false)`,
/// `ignore(false)`) so a user's unrelated ignore rules don't swallow
/// legitimate `.tera` / `.yuilink` files deeper in the tree.
pub fn source_walker(source: &Utf8Path) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(source);
    b.hidden(false).git_ignore(false).ignore(false);
    b.filter_entry(|entry| entry.file_name() != ".yui");
    b
}

/// Mirror an absolute target path into a backup directory, dropping the drive
/// colon on Windows so the path is filesystem-safe.
///
/// ```text
///   C:\Users\u\foo.yml + .yui/backup → .yui/backup/C/Users/u/foo.yml
///   /home/u/foo.yml    + .yui/backup → .yui/backup/home/u/foo.yml
/// ```
pub fn mirror_into_backup(backup_root: &Utf8Path, abs_target: &Utf8Path) -> Utf8PathBuf {
    let mut out = backup_root.to_path_buf();
    for component in abs_target.components() {
        match component {
            Utf8Component::Prefix(p) => {
                let s = p.as_str().trim_end_matches(':');
                if !s.is_empty() {
                    out.push(s);
                }
            }
            Utf8Component::RootDir | Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {}
            Utf8Component::Normal(s) => {
                out.push(s);
            }
        }
    }
    out
}

/// Append a timestamp before the extension.
///
/// ```text
///   foo/bar.yml     + ts → foo/bar_<ts>.yml
///   foo/bar         + ts → foo/bar_<ts>
///   foo/.gitconfig  + ts → foo/.gitconfig_<ts>      (treat dotfiles as stem-only)
/// ```
pub fn append_timestamp(path: &Utf8Path, ts: &str) -> Utf8PathBuf {
    let parent = path.parent().map(Utf8PathBuf::from).unwrap_or_default();
    let file_name = path.file_name().unwrap_or("");

    let (stem, ext) = match (path.file_stem(), path.extension()) {
        (Some(stem), Some(ext)) if !file_name.starts_with('.') => (stem, Some(ext)),
        _ => (file_name, None),
    };

    let new_name = match ext {
        Some(ext) => format!("{stem}_{ts}.{ext}"),
        None => format!("{stem}_{ts}"),
    };
    parent.join(new_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_unix_absolute() {
        let r = mirror_into_backup(
            Utf8Path::new("/dotfiles/.yui/backup"),
            Utf8Path::new("/home/u/.config/foo.toml"),
        );
        assert_eq!(
            r,
            Utf8PathBuf::from("/dotfiles/.yui/backup/home/u/.config/foo.toml")
        );
    }

    #[test]
    fn append_with_extension() {
        let r = append_timestamp(Utf8Path::new("a/b.yml"), "20260429_143022123");
        assert_eq!(r, Utf8PathBuf::from("a/b_20260429_143022123.yml"));
    }

    #[test]
    fn append_no_extension() {
        let r = append_timestamp(Utf8Path::new("a/b"), "20260429_143022123");
        assert_eq!(r, Utf8PathBuf::from("a/b_20260429_143022123"));
    }

    #[test]
    fn append_dotfile() {
        let r = append_timestamp(Utf8Path::new(".gitconfig"), "20260429_143022123");
        assert_eq!(r, Utf8PathBuf::from(".gitconfig_20260429_143022123"));
    }

    #[test]
    fn tilde_slash_expands() {
        let home = Utf8Path::new("/h/u");
        assert_eq!(
            expand_tilde_with("~/foo", home),
            Utf8PathBuf::from("/h/u/foo")
        );
        assert_eq!(
            expand_tilde_with("~/.config/nvim", home),
            Utf8PathBuf::from("/h/u/.config/nvim")
        );
    }

    #[test]
    fn tilde_backslash_expands_for_windows_input() {
        // Tera renders may emit Windows-style separators; accept both.
        let home = Utf8Path::new("C:/Users/u");
        assert_eq!(
            expand_tilde_with("~\\foo", home),
            Utf8PathBuf::from("C:/Users/u/foo")
        );
    }

    #[test]
    fn lone_tilde_is_home() {
        let home = Utf8Path::new("/h/u");
        assert_eq!(expand_tilde_with("~", home), Utf8PathBuf::from("/h/u"));
    }

    #[test]
    fn tilde_user_form_is_untouched() {
        let home = Utf8Path::new("/h/u");
        // We don't support `~root/...` style; leave it for the caller to
        // see a useful error (file not found) rather than silently lying.
        assert_eq!(
            expand_tilde_with("~root/foo", home),
            Utf8PathBuf::from("~root/foo")
        );
    }

    #[test]
    fn no_tilde_unchanged() {
        let home = Utf8Path::new("/h/u");
        assert_eq!(
            expand_tilde_with("/abs/path", home),
            Utf8PathBuf::from("/abs/path")
        );
        assert_eq!(
            expand_tilde_with("rel/path", home),
            Utf8PathBuf::from("rel/path")
        );
        // Mid-string `~` is not a home reference (matches POSIX/bash behaviour).
        assert_eq!(
            expand_tilde_with("/foo/~/bar", home),
            Utf8PathBuf::from("/foo/~/bar")
        );
    }
}
