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

/// One-shot `.yuiignore` test for a single path under `source`.
///
/// Builds a fresh `YuiIgnoreStack`, pushes every directory between
/// `source` and `path.parent()` (so a deeply-nested `.yuiignore`
/// participates), then asks the stack. Use this when you have a
/// single candidate path to check (e.g. manual `absorb`'s
/// mount-derived candidate); for recursive walks, push/pop on the
/// hot path with a single long-lived `YuiIgnoreStack` instead.
///
/// Patterns use full gitignore syntax: glob (`*`, `**`), negation
/// (`!`), trailing-slash dir-only matching, comments (`#`). Paths
/// outside `source` short-circuit to `false`.
///
/// If an ancestor directory is itself ignored, we return `true`
/// immediately rather than descending into its `.yuiignore` — the
/// recursive walkers (`walk_and_link`, `classify_walk_inner`) skip
/// ignored subtrees entirely, so they never see the inner rules.
/// Honouring inner whitelists here would let manual `absorb` pick a
/// path that apply / status would never have linked. (Caught in PR
/// #50 review.)
pub fn is_ignored_at(source: &Utf8Path, path: &Utf8Path, is_dir: bool) -> crate::Result<bool> {
    let Ok(rel) = path.strip_prefix(source) else {
        return Ok(false);
    };
    let mut stack = YuiIgnoreStack::new();
    stack.push_dir(source)?;
    let mut cur = source.to_owned();
    for component in rel.components() {
        let Utf8Component::Normal(c) = component else {
            continue;
        };
        cur.push(c);
        if cur == path {
            break;
        }
        if stack.is_ignored(&cur, /* is_dir */ true) {
            return Ok(true);
        }
        stack.push_dir(&cur)?;
    }
    Ok(stack.is_ignored(path, is_dir))
}

/// Build a source-tree walker that skips repo plumbing.
///
/// Excluded directory names anywhere in the tree:
///   - `.yui/` — yui's own state and backup mirror; can grow huge.
///   - `.git/` — git plumbing of the dotfiles repo itself. The
///     check is on the basename, so a `home/.config/git/` (note:
///     no leading dot) inside the dotfiles is NOT excluded — only
///     the literal `.git`.
///
/// `git_ignore(false)` / `ignore(false)` keep `.gitignore` /
/// `.ignore` rules from swallowing legitimate `.tera` / `.yuilink`
/// files deeper in the tree. `.yuiignore` is registered as a
/// custom ignore filename so the walker honours nested rules
/// (every subdir that has a `.yuiignore` adds its patterns scoped
/// to that subtree, like git does with `.gitignore`). The manual
/// recursive walks in `cmd.rs` use the `YuiIgnoreStack` companion
/// type to get the same behaviour.
pub fn source_walker(source: &Utf8Path) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(source);
    b.hidden(false).git_ignore(false).ignore(false);
    b.add_custom_ignore_filename(".yuiignore");
    b.filter_entry(|entry| {
        let name = entry.file_name();
        name != ".yui" && name != ".git"
    });
    b
}

/// Stack of `.yuiignore` matchers for manual recursive walks. Each
/// frame remembers the directory it was loaded from + the parsed
/// matcher; testing a path walks innermost → outermost so a deeper
/// `.yuiignore` overrides a shallower one (gitignore semantics).
///
/// Walkers `push_dir(d)` before iterating `d`'s entries and
/// `pop_dir(d)` once they're done with the subtree. The same
/// `YuiIgnoreStack` instance is threaded through the whole walk so
/// the stack stays consistent across recursion.
#[derive(Debug, Default)]
pub struct YuiIgnoreStack {
    layers: Vec<(Utf8PathBuf, ignore::gitignore::Gitignore)>,
}

impl YuiIgnoreStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load `.yuiignore` from `dir` (if present) and push its rules
    /// as a new layer. No-op when the file is absent.
    pub fn push_dir(&mut self, dir: &Utf8Path) -> crate::Result<()> {
        let path = dir.join(".yuiignore");
        if !path.is_file() {
            return Ok(());
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
        if let Some(e) = builder.add(path.as_std_path()) {
            return Err(crate::Error::Config(format!("parsing {path}: {e}")));
        }
        let gi = builder
            .build()
            .map_err(|e| crate::Error::Config(format!("building {path}: {e}")))?;
        self.layers.push((dir.to_owned(), gi));
        Ok(())
    }

    /// Pop the top layer if it was loaded from `dir`. Pairs with
    /// `push_dir` — calling it on a directory that didn't push a
    /// layer is a no-op.
    pub fn pop_dir(&mut self, dir: &Utf8Path) {
        if matches!(self.layers.last(), Some((p, _)) if p == dir) {
            self.layers.pop();
        }
    }

    /// Decide whether `path` should be ignored. Walks frames inside
    /// → outside; the first decisive match (Ignore or Whitelist)
    /// wins, so a deeper `.yuiignore` can both exclude *and*
    /// re-include paths the parent missed.
    pub fn is_ignored(&self, path: &Utf8Path, is_dir: bool) -> bool {
        for (anchor, gi) in self.layers.iter().rev() {
            let Ok(rel) = path.strip_prefix(anchor) else {
                continue;
            };
            match gi.matched_path_or_any_parents(rel.as_std_path(), is_dir) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => continue,
            }
        }
        false
    }
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
    fn yui_ignore_stack_root_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(root.join(".yuiignore"), "*.lock\n").unwrap();
        let mut stack = YuiIgnoreStack::new();
        stack.push_dir(&root).unwrap();
        assert!(stack.is_ignored(&root.join("foo.lock"), false));
        assert!(!stack.is_ignored(&root.join("foo.txt"), false));
        stack.pop_dir(&root);
        // After pop the matcher is gone — same path is no longer ignored.
        assert!(!stack.is_ignored(&root.join("foo.lock"), false));
    }

    #[test]
    fn yui_ignore_stack_nested_overrides_parent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(root.join(".yuiignore"), "*.lock\n").unwrap();
        // Nested re-includes everything via `!*.lock`.
        std::fs::write(inner.join(".yuiignore"), "!*.lock\n").unwrap();

        let mut stack = YuiIgnoreStack::new();
        stack.push_dir(&root).unwrap();
        assert!(stack.is_ignored(&root.join("a.lock"), false));
        stack.push_dir(&inner).unwrap();
        assert!(
            !stack.is_ignored(&inner.join("a.lock"), false),
            "deeper layer's whitelist should win"
        );
        stack.pop_dir(&inner);
        // After leaving inner, root rule applies again.
        assert!(stack.is_ignored(&root.join("b.lock"), false));
    }

    #[test]
    fn yui_ignore_stack_pop_only_matches_top() {
        // pop_dir for a directory that didn't push anything is a no-op,
        // so a missing `.yuiignore` doesn't desync the stack.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(root.join(".yuiignore"), "*.lock\n").unwrap();
        let no_ignore = root.join("plain");
        std::fs::create_dir_all(&no_ignore).unwrap();

        let mut stack = YuiIgnoreStack::new();
        stack.push_dir(&root).unwrap();
        stack.push_dir(&no_ignore).unwrap(); // no .yuiignore, no-op
        stack.pop_dir(&no_ignore); // no-op
        // Root layer is still in place.
        assert!(stack.is_ignored(&root.join("a.lock"), false));
    }

    /// A nested `!negation` cannot un-ignore a path whose ancestor
    /// directory is itself excluded — the recursive walkers never
    /// descend that subtree, so `is_ignored_at` must agree. (PR #50
    /// review caught this gap.)
    #[test]
    fn is_ignored_at_short_circuits_on_ignored_ancestor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let keepers = root.join("home").join("keepers");
        std::fs::create_dir_all(&keepers).unwrap();
        // Root excludes the entire `home/keepers/` dir.
        std::fs::write(root.join(".yuiignore"), "home/keepers/\n").unwrap();
        // Inner negation tries to re-include a single file.
        std::fs::write(keepers.join(".yuiignore"), "!wanted.lock\n").unwrap();
        // The walkers never descend into keepers/, so manual absorb
        // must agree the file is ignored.
        assert!(is_ignored_at(&root, &keepers.join("wanted.lock"), false).unwrap());
    }

    #[test]
    fn is_ignored_at_walks_intermediate_yuiignores() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let mid = root.join("mid");
        let leaf = mid.join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(mid.join(".yuiignore"), "secret*\n").unwrap();
        // mid/.yuiignore must be picked up when checking leaf/secret.txt
        assert!(is_ignored_at(&root, &leaf.join("secret.txt"), false).unwrap());
        assert!(!is_ignored_at(&root, &leaf.join("public.txt"), false).unwrap());
        // Path outside the source root is not ignored.
        let outside =
            Utf8PathBuf::from_path_buf(tmp.path().parent().unwrap().to_path_buf()).unwrap();
        assert!(!is_ignored_at(&root, &outside.join("anywhere"), false).unwrap());
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
