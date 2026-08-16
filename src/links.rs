//! `[[link]]` — explicit source→target declarations.
//!
//! Two places declare them and they share one schema:
//!
//!   - **`$DOTFILES/config.toml`** — a central `[[link]]` table. `src`
//!     is required and relative to `$DOTFILES`. Use this when you don't
//!     want marker files scattered through the tree (a marker inside a
//!     junctioned dir is also visible from the target side, which some
//!     people would rather not ship into an app's config dir).
//!   - **`<dir>/.yuilink`** — a marker. `src` is optional and relative
//!     to the marker's own directory; omitting it declares the marker's
//!     directory itself. Keeping the declaration next to the directory
//!     means it moves and dies with that directory, which a central
//!     table can't do — hence both, not one.
//!
//! Both normalize to the same shape: a list of [`DirLink`] hanging off
//! one source directory, which is exactly what the apply / status walk
//! consults when it arrives at that directory. Keying on the directory
//! is what makes coverage work — a dir-scoped link at `X` means the
//! walk must not also emit per-file links for `X`'s children.
//!
//! Marker discovery stays inside the walk (`dir_spec` reads the marker
//! for the directory it is asked about), so `.yuiignore` semantics are
//! unchanged: a marker under an ignored subtree is never read because
//! the walk never gets there. Central entries are validated up front
//! instead — `src` has to exist in the source tree, because a typo in
//! the central table has no walk-time symptom to notice.

use std::collections::{BTreeSet, HashMap};

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use tracing::warn;

use crate::config::{DirLinkMode, FileLinkMode};
use crate::marker::{self, MarkerSpec};
use crate::paths;
use crate::{Error, Result};

/// One `[[link]]` entry exactly as written by the user.
#[derive(Debug, Clone, Deserialize)]
pub struct LinkEntry {
    /// Relative path to the thing being linked. Relative to
    /// `$DOTFILES` in `config.toml`, to the marker's directory in a
    /// `.yuilink`. Omitted (markers only) = the marker's directory.
    #[serde(default)]
    pub src: Option<Utf8PathBuf>,
    /// Target path. Tera-rendered, `~` expanded.
    pub dst: String,
    /// Tera expression gating the entry.
    #[serde(default)]
    pub when: Option<String>,
    /// Link mechanism for this entry only, overriding `[mount]
    /// file_mode` / `dir_mode`. One awkward app (a watcher that
    /// resolves reparse points, an installer that rewrites in place)
    /// shouldn't force the whole repo onto a different mechanism —
    /// especially on Windows, where symlinks need Developer Mode or
    /// admin and the `auto` default exists to avoid exactly that.
    #[serde(default)]
    pub mode: Option<LinkMode>,
}

/// Per-entry `mode` as written. Which values are legal depends on what
/// the entry links: a directory can't be hardlinked, a file can't be a
/// junction. Resolution to an effective mode goes through
/// [`LinkMode::as_dir`] / [`LinkMode::as_file`], which return `None`
/// for the combination that doesn't exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    Auto,
    Symlink,
    Hardlink,
    Junction,
}

impl LinkMode {
    pub fn as_dir(self) -> Option<DirLinkMode> {
        match self {
            Self::Auto => Some(DirLinkMode::Auto),
            Self::Symlink => Some(DirLinkMode::Symlink),
            Self::Junction => Some(DirLinkMode::Junction),
            Self::Hardlink => None,
        }
    }

    pub fn as_file(self) -> Option<FileLinkMode> {
        match self {
            Self::Auto => Some(FileLinkMode::Auto),
            Self::Symlink => Some(FileLinkMode::Symlink),
            Self::Hardlink => Some(FileLinkMode::Hardlink),
            Self::Junction => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Symlink => "symlink",
            Self::Hardlink => "hardlink",
            Self::Junction => "junction",
        }
    }
}

/// Reject `mode = "hardlink"` on a directory and `mode = "junction"` on
/// a file at declaration time — both are config bugs with no sensible
/// runtime interpretation.
pub fn validate_mode(mode: LinkMode, dir_scoped: bool, label: &str) -> Result<()> {
    let ok = if dir_scoped {
        mode.as_dir().is_some()
    } else {
        mode.as_file().is_some()
    };
    if ok {
        return Ok(());
    }
    let (kind, allowed) = if dir_scoped {
        ("a directory", "auto | symlink | junction")
    } else {
        ("a file", "auto | symlink | hardlink")
    };
    Err(Error::Config(format!(
        "{label}: mode = {:?} cannot link {kind} ({allowed})",
        mode.as_str()
    )))
}

/// Diagnostic prefix for the central table. Every `config.toml`-sourced
/// message starts with it, so it lives in one place.
pub const CONFIG_LABEL: &str = "config.toml: [[link]]";

/// Where an entry was declared. Error messages only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Config,
    Marker,
}

impl Origin {
    /// Prefix for diagnostics, so a complaint says which file the user
    /// has to go fix.
    pub fn label(self, dir: &Utf8Path) -> String {
        match self {
            Self::Config => CONFIG_LABEL.to_string(),
            Self::Marker => format!("marker at {dir}: [[link]]"),
        }
    }
}

/// A `[[link]]` entry resolved against the directory it hangs off.
#[derive(Debug, Clone)]
pub struct DirLink {
    /// Path relative to the owning directory, or `None` when the entry
    /// links the directory itself.
    pub rel: Option<Utf8PathBuf>,
    /// `src` as the user wrote it. A central entry's `rel` is only the
    /// file name (it keys on the parent dir), so diagnostics quote this
    /// instead — otherwise `src=.nope` would replace `src=home/.nope`.
    pub declared: Option<Utf8PathBuf>,
    pub dst: String,
    pub when: Option<String>,
    pub origin: Origin,
    /// Per-entry mechanism override, already validated against the
    /// entry's kind.
    pub mode: Option<LinkMode>,
}

impl DirLink {
    /// `<where declared>: [[link]] src=<as written>` — the prefix for
    /// anything that goes wrong with this entry.
    pub fn describe(&self, dir: &Utf8Path) -> String {
        match &self.declared {
            Some(src) => format!("{} src={src}", self.origin.label(dir)),
            None => self.origin.label(dir),
        }
    }
}

/// Every `[[link]]` declaration that applies to one source directory.
#[derive(Debug, Default)]
pub struct DirSpec {
    /// An empty (`PassThrough`) marker was present — link this
    /// directory at the mount's natural dst.
    pub passthrough: bool,
    pub links: Vec<DirLink>,
}

/// What makes two `[[link]]` declarations "the same": target-relative
/// path, dst template, `when` guard and mechanism.
type DedupeKey = (
    Option<Utf8PathBuf>,
    String,
    Option<String>,
    Option<LinkMode>,
);

/// Central `[[link]]` entries, keyed by the source directory the walk
/// has to be standing in for them to fire.
#[derive(Debug, Default)]
pub struct LinkPlan {
    by_dir: HashMap<Utf8PathBuf, Vec<DirLink>>,
}

impl LinkPlan {
    /// Normalize `config.toml`'s `[[link]]` table.
    ///
    /// A `src` naming a directory keys on that directory itself;
    /// anything else keys on its parent so the walk emits it while
    /// iterating that directory's entries.
    ///
    /// Only the *shape* of `src` is checked here, never its existence.
    /// Two things in the source tree don't exist until `apply` creates
    /// them — `*.tera` output and `*.age` plaintext siblings, both
    /// gitignored — so a load-time existence error would make `yui list`
    /// / `status` fail on a fresh clone for a perfectly correct entry.
    /// A `when`-gated entry has the same problem in reverse. The walk
    /// checks existence *after* the `when` filter, exactly as it does
    /// for markers, which is where a typo surfaces with the path
    /// attached.
    pub fn from_config(source: &Utf8Path, entries: &[LinkEntry]) -> Result<Self> {
        let mut by_dir: HashMap<Utf8PathBuf, Vec<DirLink>> = HashMap::new();
        for entry in entries {
            let Some(src) = &entry.src else {
                return Err(Error::Config(format!(
                    "{CONFIG_LABEL} requires `src` (a path relative to $DOTFILES); \
                     only a `.yuilink` may omit it (there it means \"this directory\")"
                )));
            };
            validate_src(src, CONFIG_LABEL)?;
            let abs = source.join(src);
            let (dir, rel) = if abs.is_dir() {
                (abs, None)
            } else {
                let parent = abs.parent().map(Utf8Path::to_path_buf).ok_or_else(|| {
                    Error::Config(format!("{CONFIG_LABEL} src={src} has no parent directory"))
                })?;
                let name = abs.file_name().ok_or_else(|| {
                    Error::Config(format!("{CONFIG_LABEL} src={src} has no file name"))
                })?;
                (parent, Some(Utf8PathBuf::from(name)))
            };
            if let Some(mode) = entry.mode {
                validate_mode(mode, rel.is_none(), &format!("{CONFIG_LABEL} src={src}"))?;
            }
            by_dir.entry(dir).or_default().push(DirLink {
                rel,
                declared: Some(src.clone()),
                dst: entry.dst.clone(),
                when: entry.when.clone(),
                origin: Origin::Config,
                mode: entry.mode,
            });
        }
        Ok(Self { by_dir })
    }

    /// Merge the `.yuilink` marker at `dir` (when the mount strategy
    /// honours markers) with the central entries keyed at `dir`.
    ///
    /// Exact duplicates — same `rel`, `dst` and `when` — collapse to
    /// one link, so declaring the same mapping centrally *and* in a
    /// marker doesn't double up the work or the `status` / `list` rows.
    pub fn dir_spec(
        &self,
        dir: &Utf8Path,
        marker_filename: &str,
        honor_markers: bool,
    ) -> Result<DirSpec> {
        let mut spec = DirSpec::default();
        if honor_markers {
            match marker::read_spec(dir, marker_filename)? {
                None => {}
                Some(MarkerSpec::PassThrough) => spec.passthrough = true,
                Some(MarkerSpec::Explicit { links }) => {
                    spec.links.extend(links.into_iter().map(|e| DirLink {
                        rel: e.src.clone(),
                        declared: e.src,
                        dst: e.dst,
                        when: e.when,
                        origin: Origin::Marker,
                        mode: e.mode,
                    }));
                }
            }
        }
        if let Some(central) = self.by_dir.get(dir) {
            spec.links.extend(central.iter().cloned());
        }
        // Dedupe on everything that decides what gets linked and how:
        // the same dst reached with two different mechanisms is a
        // genuine (if odd) pair of declarations, not a duplicate.
        let mut seen: BTreeSet<DedupeKey> = BTreeSet::new();
        spec.links
            .retain(|l| seen.insert((l.rel.clone(), l.dst.clone(), l.when.clone(), l.mode)));
        Ok(spec)
    }

    /// Every source directory carrying a `[[link]]` declaration —
    /// central entries plus every `.yuilink` under `source`.
    ///
    /// For commands that don't walk mounts (`list`, `absorb`'s reverse
    /// lookup) this is the set of directories to ask [`Self::dir_spec`]
    /// about. `.yuiignore` / gitignore filtering comes from
    /// [`paths::source_walker`], so markers under ignored subtrees stay
    /// invisible here too.
    pub fn declared_dirs(&self, source: &Utf8Path, marker_filename: &str) -> BTreeSet<Utf8PathBuf> {
        let mut dirs: BTreeSet<Utf8PathBuf> = self.by_dir.keys().cloned().collect();
        for ent in paths::source_walker(source).build() {
            let Ok(ent) = ent else { continue };
            if !ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            if ent.path().file_name().and_then(|n| n.to_str()) != Some(marker_filename) {
                continue;
            }
            let Some(parent) = ent.path().parent() else {
                continue;
            };
            if let Ok(p) = Utf8PathBuf::from_path_buf(parent.to_path_buf()) {
                dirs.insert(p);
            }
        }
        dirs
    }

    /// Warn about central entries no walk will ever reach.
    ///
    /// A central entry only fires when a mount walk arrives at its
    /// directory. Point one outside every mount's `src` subtree and it
    /// parses, validates, shows up in `yui list` — and silently never
    /// links anything. That's exactly the class of mistake a central
    /// table invites (the declaration is nowhere near the tree it talks
    /// about), so say it out loud. A warning rather than an error per
    /// the resilience principle: the entry may be legitimately dormant
    /// (e.g. its subtree is `.yuiignore`d on this host), and one
    /// questionable declaration must not take the whole run down.
    pub fn warn_unreachable<'a>(&self, mount_roots: impl IntoIterator<Item = &'a Utf8Path>) {
        let roots: Vec<&Utf8Path> = mount_roots.into_iter().collect();
        for dir in self.by_dir.keys() {
            if roots
                .iter()
                .any(|root| dir == *root || dir.starts_with(root))
            {
                continue;
            }
            warn!(
                "config.toml: [[link]] under {dir} is outside every mount \
                 — no walk reaches it, so it will not be linked"
            );
        }
    }
}

/// `src` must stay inside the declaration's base directory: a relative
/// path made of plain components only.
///
/// Rejecting `..` and absolute paths is what keeps `src` meaningful —
/// an escaping `src` would link something the declaration's directory
/// doesn't own, and for a marker it would silently reach outside the
/// subtree the marker is supposed to describe.
pub fn validate_src(src: &Utf8Path, origin_label: &str) -> Result<()> {
    let raw = src.as_str();
    if raw.trim().is_empty() {
        return Err(Error::Config(format!(
            "{origin_label} src must not be empty"
        )));
    }
    let all_normal = src
        .components()
        .all(|c| matches!(c, Utf8Component::Normal(_)));
    if !all_normal || src.is_absolute() {
        return Err(Error::Config(format!(
            "{origin_label} src must be a relative path inside the declaring \
             directory (no `.`/`..`, no absolute paths), got {raw:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn root(tmp: &TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap()
    }

    fn entry(src: &str, dst: &str) -> LinkEntry {
        LinkEntry {
            src: Some(Utf8PathBuf::from(src)),
            dst: dst.to_string(),
            when: None,
            mode: None,
        }
    }

    fn entry_with_mode(src: &str, dst: &str, mode: LinkMode) -> LinkEntry {
        LinkEntry {
            mode: Some(mode),
            ..entry(src, dst)
        }
    }

    #[test]
    fn config_dir_entry_keys_on_the_directory_itself() {
        let tmp = TempDir::new().unwrap();
        let source = root(&tmp);
        std::fs::create_dir_all(source.join("home/.omp")).unwrap();
        let plan = LinkPlan::from_config(&source, &[entry("home/.omp", "/t/.omp")]).unwrap();

        let spec = plan
            .dir_spec(&source.join("home/.omp"), ".yuilink", true)
            .unwrap();
        assert_eq!(spec.links.len(), 1);
        assert!(spec.links[0].rel.is_none(), "dir-scoped entry has no rel");
        assert_eq!(spec.links[0].dst, "/t/.omp");

        // Nothing hangs off the parent — coverage must not leak upward.
        let parent = plan
            .dir_spec(&source.join("home"), ".yuilink", true)
            .unwrap();
        assert!(parent.links.is_empty());
    }

    #[test]
    fn config_file_entry_keys_on_the_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let source = root(&tmp);
        std::fs::create_dir_all(source.join("home/.config/powershell")).unwrap();
        std::fs::write(source.join("home/.config/powershell/profile.ps1"), "x").unwrap();
        let plan = LinkPlan::from_config(
            &source,
            &[entry("home/.config/powershell/profile.ps1", "/t/p.ps1")],
        )
        .unwrap();

        let spec = plan
            .dir_spec(&source.join("home/.config/powershell"), ".yuilink", true)
            .unwrap();
        assert_eq!(spec.links.len(), 1);
        assert_eq!(
            spec.links[0].rel.as_deref(),
            Some(Utf8Path::new("profile.ps1"))
        );
    }

    #[test]
    fn config_entry_without_src_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let err = LinkPlan::from_config(
            &root(&tmp),
            &[LinkEntry {
                src: None,
                dst: "/t".to_string(),
                when: None,
                mode: None,
            }],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("requires `src`"));
    }

    /// A `src` that doesn't exist yet is not a load-time error: `*.tera`
    /// output and `*.age` plaintext siblings only appear once `apply`
    /// runs, so `list` / `status` on a fresh clone must not blow up. It
    /// keys on the parent as a file entry, and the walk reports it after
    /// the `when` filter — with the path as written.
    #[test]
    fn config_entry_with_missing_src_keys_on_parent() {
        let tmp = TempDir::new().unwrap();
        let source = root(&tmp);
        std::fs::create_dir_all(source.join("home")).unwrap();
        let plan =
            LinkPlan::from_config(&source, &[entry("home/.gitconfig", "/t/.gitconfig")]).unwrap();

        let spec = plan
            .dir_spec(&source.join("home"), ".yuilink", true)
            .unwrap();
        assert_eq!(spec.links.len(), 1);
        assert_eq!(
            spec.links[0].rel.as_deref(),
            Some(Utf8Path::new(".gitconfig"))
        );
        // Diagnostics quote `src` as written, not the bare file name.
        assert_eq!(
            spec.links[0].describe(&source.join("home")),
            "config.toml: [[link]] src=home/.gitconfig"
        );
    }

    #[test]
    fn escaping_src_is_rejected() {
        for bad in ["..", "../outside", "home/../../etc", ".", "/abs/path"] {
            let err = validate_src(Utf8Path::new(bad), "test: [[link]]").unwrap_err();
            assert!(
                format!("{err}").contains("relative path"),
                "expected rejection for {bad:?}, got {err}"
            );
        }
    }

    #[test]
    fn nested_relative_src_is_accepted() {
        validate_src(Utf8Path::new("sub/dir/file.txt"), "test: [[link]]").unwrap();
    }

    /// Same mapping declared centrally and in a marker → one link, not
    /// two. Without this, `status` reports the identical pair twice.
    #[test]
    fn identical_central_and_marker_entries_collapse() {
        let tmp = TempDir::new().unwrap();
        let source = root(&tmp);
        std::fs::create_dir_all(source.join("home/.omp")).unwrap();
        std::fs::write(
            source.join("home/.omp/.yuilink"),
            "[[link]]\ndst = \"/t/.omp\"\n",
        )
        .unwrap();
        let plan = LinkPlan::from_config(&source, &[entry("home/.omp", "/t/.omp")]).unwrap();

        let spec = plan
            .dir_spec(&source.join("home/.omp"), ".yuilink", true)
            .unwrap();
        assert_eq!(spec.links.len(), 1, "duplicate declarations collapse");
    }

    /// Differing dsts stack — that's the documented marker behaviour and
    /// central entries join it rather than replacing it.
    #[test]
    fn differing_central_and_marker_entries_stack() {
        let tmp = TempDir::new().unwrap();
        let source = root(&tmp);
        std::fs::create_dir_all(source.join("home/.config/nvim")).unwrap();
        std::fs::write(
            source.join("home/.config/nvim/.yuilink"),
            "[[link]]\ndst = \"/local/nvim\"\n",
        )
        .unwrap();
        let plan = LinkPlan::from_config(&source, &[entry("home/.config/nvim", "/t/.config/nvim")])
            .unwrap();

        let spec = plan
            .dir_spec(&source.join("home/.config/nvim"), ".yuilink", true)
            .unwrap();
        assert_eq!(spec.links.len(), 2);
    }

    /// A directory can't be hardlinked and a file can't be a junction;
    /// both are caught where the entry is declared, not at link time.
    #[test]
    fn mode_is_validated_against_the_entry_kind() {
        let tmp = TempDir::new().unwrap();
        let source = root(&tmp);
        std::fs::create_dir_all(source.join("home/dir")).unwrap();
        std::fs::write(source.join("home/file.txt"), "x").unwrap();

        let err = LinkPlan::from_config(
            &source,
            &[entry_with_mode("home/dir", "/t/dir", LinkMode::Hardlink)],
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("cannot link a directory"),
            "{err}"
        );

        let err = LinkPlan::from_config(
            &source,
            &[entry_with_mode("home/file.txt", "/t/f", LinkMode::Junction)],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("cannot link a file"), "{err}");

        // The legal combinations survive.
        LinkPlan::from_config(
            &source,
            &[
                entry_with_mode("home/dir", "/t/dir", LinkMode::Symlink),
                entry_with_mode("home/file.txt", "/t/f", LinkMode::Hardlink),
            ],
        )
        .unwrap();
    }

    /// `mode` rides along to the walk, and two entries that differ only
    /// by mechanism are two declarations, not a duplicate.
    #[test]
    fn mode_reaches_the_dir_spec_and_survives_dedupe() {
        let tmp = TempDir::new().unwrap();
        let source = root(&tmp);
        std::fs::create_dir_all(source.join("home/app")).unwrap();
        let plan = LinkPlan::from_config(
            &source,
            &[
                entry("home/app", "/t/app"),
                entry_with_mode("home/app", "/t/app", LinkMode::Symlink),
            ],
        )
        .unwrap();

        let spec = plan
            .dir_spec(&source.join("home/app"), ".yuilink", true)
            .unwrap();
        assert_eq!(spec.links.len(), 2);
        assert_eq!(spec.links[0].mode, None);
        assert_eq!(spec.links[1].mode, Some(LinkMode::Symlink));
    }

    /// `per-file` mounts ignore markers; a central entry is an explicit
    /// instruction and still applies.
    #[test]
    fn markers_can_be_skipped_while_central_entries_remain() {
        let tmp = TempDir::new().unwrap();
        let source = root(&tmp);
        std::fs::create_dir_all(source.join("home/.omp")).unwrap();
        std::fs::write(source.join("home/.omp/.yuilink"), "").unwrap();
        let plan = LinkPlan::from_config(&source, &[entry("home/.omp", "/t/.omp")]).unwrap();

        let spec = plan
            .dir_spec(&source.join("home/.omp"), ".yuilink", false)
            .unwrap();
        assert!(!spec.passthrough, "marker ignored when honor_markers=false");
        assert_eq!(spec.links.len(), 1, "central entry still applies");
    }

    #[test]
    fn declared_dirs_unions_markers_and_central_entries() {
        let tmp = TempDir::new().unwrap();
        let source = root(&tmp);
        std::fs::create_dir_all(source.join("home/.config/nvim")).unwrap();
        std::fs::create_dir_all(source.join("home/.omp")).unwrap();
        std::fs::write(source.join("home/.config/nvim/.yuilink"), "").unwrap();
        let plan = LinkPlan::from_config(&source, &[entry("home/.omp", "/t/.omp")]).unwrap();

        let dirs = plan.declared_dirs(&source, ".yuilink");
        assert!(dirs.contains(&source.join("home/.config/nvim")));
        assert!(dirs.contains(&source.join("home/.omp")));
        assert_eq!(dirs.len(), 2);
    }

    /// A central entry outside every mount subtree parses fine but no
    /// walk reaches it. `warn_unreachable` is what tells the user; here
    /// we pin the reachability rule it applies (`==` or descendant),
    /// since that's the part worth regressing on.
    #[test]
    fn reachability_covers_the_dir_itself_and_descendants() {
        let tmp = TempDir::new().unwrap();
        let source = root(&tmp);
        std::fs::create_dir_all(source.join("home/.config/nvim")).unwrap();
        std::fs::create_dir_all(source.join("elsewhere")).unwrap();
        let plan = LinkPlan::from_config(
            &source,
            &[
                entry("home/.config/nvim", "/t/nvim"),
                entry("elsewhere", "/t/elsewhere"),
            ],
        )
        .unwrap();

        let mount_root = source.join("home");
        let reachable: Vec<&Utf8PathBuf> = plan
            .by_dir
            .keys()
            .filter(|d| *d == &mount_root || d.starts_with(&mount_root))
            .collect();
        assert_eq!(reachable.len(), 1, "only the entry under home/ is reached");
        assert!(reachable[0].ends_with("nvim"));

        // Doesn't panic, and the unreachable one is what it complains
        // about (message content is a log line, not a contract).
        plan.warn_unreachable([mount_root.as_path()]);
    }
}
