//! Drift detection — classify the relationship between a source file/dir
//! and its target counterpart.
//!
//! Used by:
//!   - `yui status` (display the classification)
//!   - `yui apply` (decide what to do: skip / relink / auto-absorb / ask)
//!
//! ## Decision matrix
//!
//! | target state                                          | classify result |
//! |-------------------------------------------------------|-----------------|
//! | resolves to the same inode/file-id as source          | `InSync`        |
//! | different inode but **identical content**             | `RelinkOnly`    |
//! | different + content differs + target.mtime > source's | `AutoAbsorb`    |
//! | different + content differs + source.mtime ≥ target's | `NeedsConfirm`  |
//! | target missing                                        | `Restore`       |
//!
//! "Same inode" is computed via the `same-file` crate, which transparently
//! follows symlinks, hardlinks, and junctions on Windows.

use camino::Utf8Path;
use same_file::Handle;

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsorbDecision {
    /// `target` resolves to `source` already — link is intact.
    InSync,
    /// Inode broken but contents identical — `apply` can relink without
    /// touching source.
    RelinkOnly,
    /// `target.mtime > source.mtime` and content differs — `apply` should
    /// back up source, copy target → source, then relink. The user
    /// edited the live copy and we honor that.
    AutoAbsorb,
    /// `source.mtime ≥ target.mtime` and content differs — anomaly
    /// (source updated since last apply but target was also touched);
    /// `apply` defers to `[absorb] on_anomaly` policy.
    NeedsConfirm,
    /// `target` is missing — `apply` simply links from source.
    Restore,
}

pub fn classify(source: &Utf8Path, target: &Utf8Path) -> Result<AbsorbDecision> {
    // Target gone? — restore from source.
    let target_meta = match std::fs::symlink_metadata(target) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AbsorbDecision::Restore);
        }
        Err(e) => return Err(e.into()),
    };

    // Inode/file-id comparison (follows symlinks and junctions). If both
    // resolve to the same backing file, the link is intact regardless of
    // which mechanism made it (hardlink, symlink, junction).
    if let (Ok(src_h), Ok(dst_h)) = (
        Handle::from_path(source.as_std_path()),
        Handle::from_path(target.as_std_path()),
    ) {
        if src_h == dst_h {
            return Ok(AbsorbDecision::InSync);
        }
    }

    // Directories: we don't deep-compare contents — drift on a junction'd
    // directory is unusual enough to warrant a manual look.
    let source_meta = std::fs::metadata(source)?;
    if target_meta.file_type().is_dir() && source_meta.file_type().is_dir() {
        return Ok(AbsorbDecision::NeedsConfirm);
    }

    // File vs file: compare content + mtime to choose the action. Fast
    // path: compare sizes first to avoid loading two arbitrary blobs into
    // memory whenever the answer is obviously "different".
    if target_meta.file_type().is_file() && source_meta.file_type().is_file() {
        let identical = source_meta.len() == target_meta.len()
            && std::fs::read(source)? == std::fs::read(target)?;
        if identical {
            return Ok(AbsorbDecision::RelinkOnly);
        }
        let src_mtime = source_meta.modified()?;
        let dst_mtime = target_meta.modified()?;
        if dst_mtime > src_mtime {
            return Ok(AbsorbDecision::AutoAbsorb);
        }
        return Ok(AbsorbDecision::NeedsConfirm);
    }

    // Type mismatch (file vs dir, or one is a broken/odd link) — anomaly.
    Ok(AbsorbDecision::NeedsConfirm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    fn utf8(p: std::path::PathBuf) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(p).unwrap()
    }

    /// Set a file's mtime to a specific instant. `set_modified` requires a
    /// writable file handle, which `File::open` doesn't grant on Windows.
    fn backdate(path: &Utf8Path, when: SystemTime) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open writable for set_modified");
        f.set_modified(when).expect("set_modified");
    }

    #[test]
    fn missing_target_is_restore() {
        let tmp = TempDir::new().unwrap();
        let src = utf8(tmp.path().join("src.txt"));
        std::fs::write(&src, "x").unwrap();
        let dst = utf8(tmp.path().join("dst.txt"));
        assert_eq!(classify(&src, &dst).unwrap(), AbsorbDecision::Restore);
    }

    #[test]
    fn hardlink_is_in_sync() {
        let tmp = TempDir::new().unwrap();
        let src = utf8(tmp.path().join("src.txt"));
        std::fs::write(&src, "x").unwrap();
        let dst = utf8(tmp.path().join("dst.txt"));
        std::fs::hard_link(&src, &dst).unwrap();
        assert_eq!(classify(&src, &dst).unwrap(), AbsorbDecision::InSync);
    }

    #[test]
    fn separate_files_same_content_is_relink_only() {
        let tmp = TempDir::new().unwrap();
        let src = utf8(tmp.path().join("src.txt"));
        let dst = utf8(tmp.path().join("dst.txt"));
        std::fs::write(&src, "same body").unwrap();
        std::fs::write(&dst, "same body").unwrap();
        assert_eq!(classify(&src, &dst).unwrap(), AbsorbDecision::RelinkOnly);
    }

    #[test]
    fn target_newer_with_diff_is_auto_absorb() {
        let tmp = TempDir::new().unwrap();
        let src = utf8(tmp.path().join("src.txt"));
        let dst = utf8(tmp.path().join("dst.txt"));
        std::fs::write(&src, "old source").unwrap();
        // Make sure mtimes are clearly ordered: backdate source, then write dst fresh.
        let past = SystemTime::now() - Duration::from_secs(60);
        backdate(&src, past);
        std::fs::write(&dst, "edited target").unwrap();
        assert_eq!(classify(&src, &dst).unwrap(), AbsorbDecision::AutoAbsorb);
    }

    #[test]
    fn source_newer_with_diff_is_needs_confirm() {
        let tmp = TempDir::new().unwrap();
        let src = utf8(tmp.path().join("src.txt"));
        let dst = utf8(tmp.path().join("dst.txt"));
        std::fs::write(&dst, "old target").unwrap();
        let past = SystemTime::now() - Duration::from_secs(60);
        backdate(&dst, past);
        std::fs::write(&src, "fresh source").unwrap();
        assert_eq!(classify(&src, &dst).unwrap(), AbsorbDecision::NeedsConfirm);
    }

    #[test]
    fn separate_dirs_are_needs_confirm() {
        let tmp = TempDir::new().unwrap();
        let src = utf8(tmp.path().join("src_dir"));
        let dst = utf8(tmp.path().join("dst_dir"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        assert_eq!(classify(&src, &dst).unwrap(), AbsorbDecision::NeedsConfirm);
    }
}
