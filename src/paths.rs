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

/// Resolve a `[[mount.entry]] src = "..."` value to an absolute
/// path. `~` and `~/...` expand to the home directory; absolute
/// inputs are returned verbatim (so a private clone at
/// `~/.dotfiles-private/home` mounts directly without symlinking).
/// Anything else is treated as relative to `source` (the dotfiles
/// repo root) — the historical default.
///
/// Implementation detail: `Utf8PathBuf::join` already replaces the
/// base with absolute arguments, so we just need `expand_tilde`
/// first to handle the `~` form.
///
/// If `expand_tilde` couldn't resolve `~` (both `HOME` and
/// `USERPROFILE` unset), it returns the input verbatim — in that
/// case we MUST NOT rebase it under `source`, because
/// `<source>/~/foo` silently resolves to a wrong tree instead of
/// surfacing the unset-home configuration error. Return the
/// unresolved `~` form so the caller hits a clear "no such path"
/// later. (Caught in PR #56 review by coderabbitai.)
pub fn resolve_mount_src(source: &Utf8Path, src: &str) -> Utf8PathBuf {
    resolve_mount_src_with(source, src, home_dir().as_deref())
}

/// `resolve_mount_src` with an explicit home — used in tests so we
/// don't mutate the process-wide `HOME` env var (which races with
/// parallel tests). Production code goes through `resolve_mount_src`.
pub fn resolve_mount_src_with(
    source: &Utf8Path,
    src: &str,
    home: Option<&Utf8Path>,
) -> Utf8PathBuf {
    let expanded = match home {
        Some(h) => expand_tilde_with(src, h),
        None => Utf8PathBuf::from(src),
    };
    let is_tilde_form = src == "~" || src.starts_with("~/") || src.starts_with("~\\");
    if is_tilde_form && expanded.as_str() == src {
        // No home to resolve against — return the unresolved form
        // verbatim so the caller surfaces a clear "no such path"
        // rather than a silent rebase under `source`.
        return expanded;
    }
    source.join(expanded)
}

/// One-shot ignore test for a single path under `source`.
///
/// Builds a fresh `YuiIgnoreStack`, pushes every directory between
/// `source` and `path.parent()` (so a deeply-nested `.yuiignore`
/// participates), then asks the stack. Use this when you have a
/// single candidate path to check (e.g. manual `absorb`'s
/// mount-derived candidate); for recursive walks, push/pop on the
/// hot path with a single long-lived `YuiIgnoreStack` instead.
///
/// `respect_gitignore` layers `.gitignore` under `.yuiignore` — see
/// [`YuiIgnoreStack::with_gitignore`] for the rationale and for how
/// yui's own managed section is exempted.
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
pub fn is_ignored_at(
    source: &Utf8Path,
    path: &Utf8Path,
    is_dir: bool,
    respect_gitignore: bool,
) -> crate::Result<bool> {
    let Ok(rel) = path.strip_prefix(source) else {
        return Ok(false);
    };
    let mut stack = YuiIgnoreStack::with_gitignore(respect_gitignore);
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

/// Stack of ignore matchers for manual recursive walks. Each frame
/// remembers the directory it was loaded from + that directory's
/// matchers; testing a path walks innermost → outermost so a deeper
/// `.yuiignore` overrides a shallower one (gitignore semantics).
///
/// Walkers `push_dir(d)` before iterating `d`'s entries and
/// `pop_dir(d)` once they're done with the subtree. The same
/// `YuiIgnoreStack` instance is threaded through the whole walk so
/// the stack stays consistent across recursion.
#[derive(Debug, Default)]
pub struct YuiIgnoreStack {
    /// Matchers for one directory, ordered most-specific-first:
    /// `.yuiignore` before `.gitignore`, so a `.yuiignore` rule can
    /// re-include (`!foo`) something the sibling `.gitignore` excluded.
    layers: Vec<(Utf8PathBuf, Vec<ignore::gitignore::Gitignore>)>,
    respect_gitignore: bool,
}

impl YuiIgnoreStack {
    /// Stack that honours `.yuiignore` only.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stack that additionally honours each directory's `.gitignore`
    /// when `respect_gitignore` is set.
    ///
    /// Rationale: yui is target-as-truth, so the source tree *is* a
    /// git repo and apps write runtime state (session logs, caches,
    /// credentials) straight into it through the links. Those files
    /// are already excluded from git; treating them as managed
    /// link candidates only buys per-file hardlinks nobody wants and
    /// a `yui status` drowned in noise.
    ///
    /// **Yui's own managed section is exempt.** `render.rs` lists
    /// every rendered `.tera` output and decrypted `.age` plaintext
    /// between the `# >>> yui rendered (auto-managed…) >>>` markers
    /// — those files are gitignored precisely *because* yui
    /// generates them, and they must still be linked. Lines inside
    /// the markers are dropped before the matcher is built, so
    /// enabling this can never stop apply from linking a generated
    /// file.
    pub fn with_gitignore(respect_gitignore: bool) -> Self {
        Self {
            layers: Vec::new(),
            respect_gitignore,
        }
    }

    /// Load `dir`'s ignore files (if present) and push them as one
    /// layer. No-op when the directory has none.
    pub fn push_dir(&mut self, dir: &Utf8Path) -> crate::Result<()> {
        let mut matchers = Vec::new();
        if let Some(gi) =
            build_matcher(dir, &dir.join(".yuiignore"), /* strip_managed */ false)?
        {
            matchers.push(gi);
        }
        if self.respect_gitignore {
            if let Some(gi) =
                build_matcher(dir, &dir.join(".gitignore"), /* strip_managed */ true)?
            {
                matchers.push(gi);
            }
        }
        if !matchers.is_empty() {
            self.layers.push((dir.to_owned(), matchers));
        }
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
    /// → outside (and within a frame, `.yuiignore` before
    /// `.gitignore`); the first decisive match (Ignore or Whitelist)
    /// wins, so a deeper `.yuiignore` can both exclude *and*
    /// re-include paths the parent missed.
    pub fn is_ignored(&self, path: &Utf8Path, is_dir: bool) -> bool {
        for (anchor, matchers) in self.layers.iter().rev() {
            let Ok(rel) = path.strip_prefix(anchor) else {
                continue;
            };
            for gi in matchers {
                match gi.matched_path_or_any_parents(rel.as_std_path(), is_dir) {
                    ignore::Match::Ignore(_) => return true,
                    ignore::Match::Whitelist(_) => return false,
                    ignore::Match::None => continue,
                }
            }
        }
        false
    }
}

/// Parse one ignore file into a matcher anchored at `dir`, or `None`
/// when the file doesn't exist.
///
/// `strip_managed` drops the lines yui itself wrote between the
/// `.gitignore` managed-section markers — see
/// [`YuiIgnoreStack::with_gitignore`]. Because that filtering happens
/// line-by-line we feed the builder via `add_line` rather than
/// `add`, which is also why blank lines and comments are passed
/// straight through (the builder skips them itself).
fn build_matcher(
    dir: &Utf8Path,
    path: &Utf8Path,
    strip_managed: bool,
) -> crate::Result<Option<ignore::gitignore::Gitignore>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path.as_std_path())
        .map_err(|e| crate::Error::Config(format!("reading {path}: {e}")))?;

    let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
    let mut in_managed = false;
    for line in text.lines() {
        if strip_managed {
            let trimmed = line.trim();
            if trimmed == crate::render::GITIGNORE_BEGIN {
                in_managed = true;
                continue;
            }
            if trimmed == crate::render::GITIGNORE_END {
                in_managed = false;
                continue;
            }
            if in_managed {
                continue;
            }
        }
        builder
            .add_line(Some(path.as_std_path().to_owned()), line)
            .map_err(|e| crate::Error::Config(format!("parsing {path}: {e}")))?;
    }
    let gi = builder
        .build()
        .map_err(|e| crate::Error::Config(format!("building {path}: {e}")))?;
    Ok(Some(gi))
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

/// Normalize a path by eliminating redundant `.` components without accessing
/// the filesystem, preserving `..` to maintain symlink semantics.
pub fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            _ => components.push(component),
        }
    }
    if components.is_empty() {
        Utf8PathBuf::from(".")
    } else {
        components.into_iter().collect()
    }
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

    /// Regression for PR #56 review: when neither `HOME` nor
    /// `USERPROFILE` is set, `expand_tilde` is a no-op and returns
    /// `~/foo` verbatim. `resolve_mount_src` must NOT then rebase
    /// it under `source` (which would yield `<source>/~/foo`,
    /// silently pointing at a wrong tree). Instead, return the
    /// unresolved `~/foo` so the caller's "no such path" error is
    /// the clear failure mode.
    #[test]
    fn resolve_mount_src_preserves_unresolved_tilde() {
        let source = Utf8Path::new("/dot");
        // `home: None` simulates HOME / USERPROFILE both unset.
        assert_eq!(
            resolve_mount_src_with(source, "~/foo", None),
            Utf8PathBuf::from("~/foo"),
        );
        assert_eq!(
            resolve_mount_src_with(source, "~", None),
            Utf8PathBuf::from("~"),
        );
    }

    /// When home is available, `~/foo` resolves to the absolute
    /// `<HOME>/foo` and `Utf8PathBuf::join` correctly drops
    /// `source` (absolute argument replaces the base).
    #[test]
    fn resolve_mount_src_expands_tilde_when_home_set() {
        let source = Utf8Path::new("/dot");
        let home = Utf8Path::new("/h/u");
        assert_eq!(
            resolve_mount_src_with(source, "~/private/home", Some(home)),
            Utf8PathBuf::from("/h/u/private/home"),
        );
        assert_eq!(
            resolve_mount_src_with(source, "~", Some(home)),
            Utf8PathBuf::from("/h/u"),
        );
    }

    /// Plain relative input keeps the historical "join under
    /// source" behaviour.
    #[test]
    fn resolve_mount_src_relative_joins_under_source() {
        let source = Utf8Path::new("/dot");
        assert_eq!(
            resolve_mount_src_with(source, "home", Some(Utf8Path::new("/h/u"))),
            Utf8PathBuf::from("/dot/home"),
        );
    }

    /// Absolute input is returned verbatim (Path::join with
    /// absolute replaces the base).
    #[test]
    fn resolve_mount_src_absolute_returns_verbatim() {
        let source = Utf8Path::new("/dot");
        assert_eq!(
            resolve_mount_src_with(source, "/abs/private/home", Some(Utf8Path::new("/h/u"))),
            Utf8PathBuf::from("/abs/private/home"),
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
        assert!(is_ignored_at(&root, &keepers.join("wanted.lock"), false, false).unwrap());
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
        assert!(is_ignored_at(&root, &leaf.join("secret.txt"), false, false).unwrap());
        assert!(!is_ignored_at(&root, &leaf.join("public.txt"), false, false).unwrap());
        // Path outside the source root is not ignored.
        let outside =
            Utf8PathBuf::from_path_buf(tmp.path().parent().unwrap().to_path_buf()).unwrap();
        assert!(!is_ignored_at(&root, &outside.join("anywhere"), false, false).unwrap());
    }

    #[test]
    fn is_ignored_at_respects_gitignore() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let mid = root.join("mid");
        let leaf = mid.join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();

        // Write .gitignore excluding secret.txt
        std::fs::write(mid.join(".gitignore"), "secret.txt\n").unwrap();

        // When respect_gitignore is false, it's not ignored
        assert!(!is_ignored_at(&root, &leaf.join("secret.txt"), false, false).unwrap());
        // When respect_gitignore is true, it is ignored
        assert!(is_ignored_at(&root, &leaf.join("secret.txt"), false, true).unwrap());

        // Test that yui's auto-managed section in .gitignore is exempt
        let gitignore_content = format!(
            "ignored.txt\n{}\nexempt.txt\n{}\n",
            crate::render::GITIGNORE_BEGIN,
            crate::render::GITIGNORE_END
        );
        std::fs::write(mid.join(".gitignore"), gitignore_content).unwrap();

        // ignored.txt is ignored
        assert!(is_ignored_at(&root, &mid.join("ignored.txt"), false, true).unwrap());
        // exempt.txt is inside the markers, so it should not be ignored
        assert!(!is_ignored_at(&root, &mid.join("exempt.txt"), false, true).unwrap());
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

    #[test]
    fn normalize_paths() {
        assert_eq!(
            normalize(Utf8Path::new("home/.config/gcal/./credentials.json")),
            Utf8PathBuf::from("home/.config/gcal/credentials.json")
        );
        assert_eq!(
            normalize(Utf8Path::new("a/b/./c/./d")),
            Utf8PathBuf::from("a/b/c/d")
        );
        assert_eq!(normalize(Utf8Path::new("./foo")), Utf8PathBuf::from("foo"));
        assert_eq!(normalize(Utf8Path::new(".")), Utf8PathBuf::from("."));
    }
}
