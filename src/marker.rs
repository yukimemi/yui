//! `.yuilink` marker file detection + parsing.
//!
//! Two forms are accepted:
//!   - **empty file** → "junction this dir at the parent mount's dst"
//!     (the original presence-only marker semantics)
//!   - **TOML with `[[link]]` entries** → "junction this dir at each
//!     entry's `dst`, when filtered" — overrides the parent mount's dst.
//!
//! ```toml
//! # $DOTFILES/home/.config/nvim/.yuilink
//! [[link]]
//! dst = "{{ env(name='HOME') }}/.config/nvim"
//!
//! [[link]]
//! dst = "{{ env(name='LOCALAPPDATA') }}/nvim"
//! when = "{{ yui.os == 'windows' }}"
//! ```

use camino::Utf8Path;
use serde::Deserialize;

use crate::{Error, Result};

#[derive(Debug, Clone)]
pub enum MarkerSpec {
    /// Empty marker — link this dir using the parent mount's dst.
    PassThrough,
    /// Per-dir override; each entry produces a link (after `when` filter).
    /// The parent mount's dst is bypassed for this directory.
    Override { links: Vec<MarkerLink> },
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarkerLink {
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
///   - `Ok(Some(Override { ... }))` — present with `[[link]]` entries
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
    Ok(Some(MarkerSpec::Override { links: parsed.link }))
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
    fn marker_with_links_is_override() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".yuilink"),
            r#"
[[link]]
dst = "/a"

[[link]]
dst = "/b"
when = "{{ yui.os == 'windows' }}"
"#,
        )
        .unwrap();
        let spec = read_spec(&root(&tmp), ".yuilink").unwrap().unwrap();
        match spec {
            MarkerSpec::Override { links } => {
                assert_eq!(links.len(), 2);
                assert_eq!(links[0].dst, "/a");
                assert!(links[0].when.is_none());
                assert_eq!(links[1].dst, "/b");
                assert_eq!(links[1].when.as_deref(), Some("{{ yui.os == 'windows' }}"));
            }
            _ => panic!("expected Override"),
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
