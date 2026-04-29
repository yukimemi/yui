//! `.yuilink` marker file detection + parsing.
//!
//! Two forms are accepted:
//!   - **empty file** → "junction this dir at the parent mount's dst"
//!     (the original presence-only marker semantics)
//!   - **TOML with `[[link]]` entries** → declare explicit links from this
//!     directory. Each entry produces one link (after `when` filter).
//!
//! ```toml
//! # $DOTFILES/home/.config/nvim/.yuilink
//! [[link]]
//! dst = "{{ env(name='HOME') }}/.config/nvim"
//!
//! [[link]]
//! dst = "{{ env(name='LOCALAPPDATA') }}/nvim"
//! when = "yui.os == 'windows'"
//! ```
//!
//! Each `[[link]]` may carry an optional `src = "<filename>"` that scopes
//! the link to a specific file inside the marker's directory rather than
//! the directory itself:
//!
//! ```toml
//! # $DOTFILES/home/.config/powershell/.yuilink
//! [[link]]
//! src = "profile.ps1"
//! dst = "{{ env(name='USERPROFILE') }}/Documents/PowerShell/Microsoft.PowerShell_profile.ps1"
//! when = "yui.os == 'windows'"
//! ```
//!
//! Stacking semantics (v0.6+): a marker no longer stops the walker. The
//! walker keeps descending past markers and aggregates link entries from
//! every marker it encounters. A descendant marker therefore *adds*
//! destinations on top of its ancestors rather than replacing them. Each
//! entry's `dst` is still the source of truth — if you want the default
//! `~/.config/nvim`-style placement, list it explicitly.
//!
//! Default-dst behaviour, two cases (kept distinct on purpose):
//!
//!   - **Empty / link-less marker** — the walker still emits the
//!     dir-level link to the parent mount's natural dst (the original
//!     "presence-only" behaviour).
//!   - **Directory-scoped `[[link]]`** (no `src`) — fully defines the
//!     directory's placement. The parent mount's natural dst is *not*
//!     implied; only what's listed here is linked at this dir.
//!   - **File-scoped `[[link]]`** (with `src = "<filename>"`) — applies
//!     only to the named sibling file. It does *not* claim
//!     directory-level coverage, so per-file defaults from the parent
//!     mount still apply to the rest of the dir (and to the same file
//!     too, in addition to the explicit dst).

use camino::Utf8Path;
use serde::Deserialize;

use crate::{Error, Result};

#[derive(Debug, Clone)]
pub enum MarkerSpec {
    /// Empty marker — link this dir using the parent mount's natural dst.
    PassThrough,
    /// Explicit links. Each entry maps the marker's directory (or a
    /// specific file inside it via `src`) to a destination.
    Explicit { links: Vec<MarkerLink> },
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarkerLink {
    /// Optional file scope. When set, this entry links the file at
    /// `<marker-dir>/<src>` to `dst` instead of the directory itself.
    /// Must be a single component (no path separators) so it stays a
    /// sibling file of the marker.
    #[serde(default)]
    pub src: Option<String>,
    pub dst: String,
    #[serde(default)]
    pub when: Option<String>,
}

#[derive(Deserialize)]
struct MarkerFile {
    #[serde(default)]
    link: Vec<MarkerLink>,
}

/// Read and parse a `.yuilink` from `dir`.
///
/// Returns:
///   - `Ok(None)` — no marker file present
///   - `Ok(Some(PassThrough))` — present and empty / whitespace-only / no `[[link]]`
///   - `Ok(Some(Explicit { ... }))` — present with `[[link]]` entries
///   - `Err(_)` — present but malformed TOML, or other IO error
pub fn read_spec(dir: &Utf8Path, marker_filename: &str) -> Result<Option<MarkerSpec>> {
    let path = dir.join(marker_filename);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    if raw.trim().is_empty() {
        return Ok(Some(MarkerSpec::PassThrough));
    }
    let parsed: MarkerFile =
        toml::from_str(&raw).map_err(|e| Error::Config(format!("parse {path}: {e}")))?;
    if parsed.link.is_empty() {
        return Ok(Some(MarkerSpec::PassThrough));
    }
    for link in &parsed.link {
        if let Some(src) = &link.src {
            // Reject anything that isn't a plain sibling file. `.` /
            // `..` would point at the marker dir itself or its parent,
            // and path separators would let the entry escape the dir
            // entirely — neither matches the "single filename" promise.
            if src.is_empty()
                || src == "."
                || src == ".."
                || src.contains('/')
                || src.contains('\\')
            {
                return Err(Error::Config(format!(
                    "parse {path}: [[link]] src must be a single filename (no path separators or `.`/`..`), got {src:?}"
                )));
            }
        }
    }
    Ok(Some(MarkerSpec::Explicit { links: parsed.link }))
}

/// Presence-only check: any `.yuilink` file (empty or with content) counts.
/// Kept for callers that don't need the spec contents.
pub fn is_marker_dir(dir: &Utf8Path, marker_filename: &str) -> bool {
    dir.join(marker_filename).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn root(tmp: &TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap()
    }

    #[test]
    fn no_marker_returns_none() {
        let tmp = TempDir::new().unwrap();
        assert!(read_spec(&root(&tmp), ".yuilink").unwrap().is_none());
    }

    #[test]
    fn empty_marker_is_passthrough() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".yuilink"), "").unwrap();
        assert!(matches!(
            read_spec(&root(&tmp), ".yuilink").unwrap(),
            Some(MarkerSpec::PassThrough)
        ));
    }

    #[test]
    fn whitespace_only_marker_is_passthrough() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".yuilink"), "  \n\n").unwrap();
        assert!(matches!(
            read_spec(&root(&tmp), ".yuilink").unwrap(),
            Some(MarkerSpec::PassThrough)
        ));
    }

    #[test]
    fn marker_with_links_is_explicit() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".yuilink"),
            r#"
[[link]]
dst = "/a"

[[link]]
dst = "/b"
when = "yui.os == 'windows'"
"#,
        )
        .unwrap();
        let spec = read_spec(&root(&tmp), ".yuilink").unwrap().unwrap();
        match spec {
            MarkerSpec::Explicit { links } => {
                assert_eq!(links.len(), 2);
                assert!(links[0].src.is_none());
                assert_eq!(links[0].dst, "/a");
                assert!(links[0].when.is_none());
                assert!(links[1].src.is_none());
                assert_eq!(links[1].dst, "/b");
                assert_eq!(links[1].when.as_deref(), Some("yui.os == 'windows'"));
            }
            _ => panic!("expected Explicit"),
        }
    }

    #[test]
    fn marker_with_file_src_parses() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".yuilink"),
            r#"
[[link]]
src = "profile.ps1"
dst = "~/Documents/PowerShell/Microsoft.PowerShell_profile.ps1"
when = "yui.os == 'windows'"
"#,
        )
        .unwrap();
        let spec = read_spec(&root(&tmp), ".yuilink").unwrap().unwrap();
        match spec {
            MarkerSpec::Explicit { links } => {
                assert_eq!(links.len(), 1);
                assert_eq!(links[0].src.as_deref(), Some("profile.ps1"));
                assert_eq!(
                    links[0].dst,
                    "~/Documents/PowerShell/Microsoft.PowerShell_profile.ps1"
                );
            }
            _ => panic!("expected Explicit"),
        }
    }

    #[test]
    fn marker_src_with_path_separator_errors() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".yuilink"),
            r#"
[[link]]
src = "sub/file.txt"
dst = "/anywhere"
"#,
        )
        .unwrap();
        let err = read_spec(&root(&tmp), ".yuilink").unwrap_err();
        assert!(format!("{err}").contains("single filename"));
    }

    #[test]
    fn marker_src_dot_or_dotdot_errors() {
        // `.` / `..` would silently escape the marker dir or point at
        // the dir itself; neither is what `[[link]] src` is for.
        for bad in [".", ".."] {
            let tmp = TempDir::new().unwrap();
            std::fs::write(
                tmp.path().join(".yuilink"),
                format!(
                    r#"
[[link]]
src = "{bad}"
dst = "/anywhere"
"#
                ),
            )
            .unwrap();
            let err = read_spec(&root(&tmp), ".yuilink").unwrap_err();
            assert!(
                format!("{err}").contains("single filename"),
                "expected rejection for src = {bad:?}, got {err}"
            );
        }
    }

    #[test]
    fn marker_with_invalid_toml_errors() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".yuilink"), "this is not toml ===").unwrap();
        let err = read_spec(&root(&tmp), ".yuilink").unwrap_err();
        assert!(format!("{err}").contains("parse"));
    }

    #[test]
    fn empty_link_array_is_passthrough() {
        // Has a [[link]] header but no entries (rare but valid TOML).
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".yuilink"), "# no links\n").unwrap();
        assert!(matches!(
            read_spec(&root(&tmp), ".yuilink").unwrap(),
            Some(MarkerSpec::PassThrough)
        ));
    }
}
