//! Cross-platform link operations.
//!
//! Mode resolution:
//!   - file `auto` → Unix=symlink, Windows=hardlink
//!   - dir  `auto` → Unix=symlink, Windows=junction
//!
//! On Windows, file symlinks need Developer Mode or admin; the default
//! `auto` (hardlink + junction) avoids that requirement entirely.

use camino::Utf8Path;

use crate::config::{DirLinkMode, FileLinkMode};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveFileMode {
    Symlink,
    Hardlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveDirMode {
    Symlink,
    Junction,
}

pub fn resolve_file_mode(mode: FileLinkMode) -> EffectiveFileMode {
    match mode {
        FileLinkMode::Symlink => EffectiveFileMode::Symlink,
        FileLinkMode::Hardlink => EffectiveFileMode::Hardlink,
        FileLinkMode::Auto => {
            if cfg!(windows) {
                EffectiveFileMode::Hardlink
            } else {
                EffectiveFileMode::Symlink
            }
        }
    }
}

pub fn resolve_dir_mode(mode: DirLinkMode) -> EffectiveDirMode {
    match mode {
        DirLinkMode::Symlink => EffectiveDirMode::Symlink,
        DirLinkMode::Junction => EffectiveDirMode::Junction,
        DirLinkMode::Auto => {
            if cfg!(windows) {
                EffectiveDirMode::Junction
            } else {
                EffectiveDirMode::Symlink
            }
        }
    }
}

pub fn link_file(src: &Utf8Path, dst: &Utf8Path, mode: EffectiveFileMode) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match mode {
        EffectiveFileMode::Hardlink => std::fs::hard_link(src, dst)?,
        EffectiveFileMode::Symlink => create_file_symlink(src, dst)?,
    }
    Ok(())
}

pub fn link_dir(src: &Utf8Path, dst: &Utf8Path, mode: EffectiveDirMode) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match mode {
        EffectiveDirMode::Junction => create_junction(src, dst)?,
        EffectiveDirMode::Symlink => create_dir_symlink(src, dst)?,
    }
    Ok(())
}

/// Remove a yui-managed link. No-op if the path doesn't exist. Refuses to
/// recursively delete a regular directory with contents.
pub fn unlink(dst: &Utf8Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(dst) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    let ft = meta.file_type();

    if ft.is_symlink() {
        #[cfg(windows)]
        {
            // Windows quirk: junctions report as is_symlink()=true,
            // is_dir()=false, is_file()=false. Try junction::delete first
            // (handles real junctions, may leave the empty dir entry behind
            // depending on Windows version — clean it up after); fall back
            // to remove_file / remove_dir for genuine file/dir symlinks.
            if junction::delete(dst.as_std_path()).is_ok() {
                let _ = std::fs::remove_dir(dst);
                return Ok(());
            }
            if std::fs::remove_file(dst).is_ok() {
                return Ok(());
            }
            std::fs::remove_dir(dst)?;
            return Ok(());
        }
        #[cfg(unix)]
        {
            std::fs::remove_file(dst)?;
            return Ok(());
        }
    }

    if ft.is_dir() {
        #[cfg(windows)]
        {
            return remove_link_dir_windows(dst);
        }
        #[cfg(unix)]
        return std::fs::remove_dir(dst).map_err(|e| {
            Error::Other(anyhow::anyhow!(
                "unlink: {dst} not removed as a directory link (regular dir with content?): {e}"
            ))
        });
    }

    // regular file (or a hardlink — indistinguishable, removing only drops
    // this name from the inode's link count).
    std::fs::remove_file(dst)?;
    Ok(())
}

#[cfg(unix)]
fn create_file_symlink(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    std::os::unix::fs::symlink(src, dst)?;
    Ok(())
}

#[cfg(unix)]
fn create_dir_symlink(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    std::os::unix::fs::symlink(src, dst)?;
    Ok(())
}

#[cfg(unix)]
fn create_junction(_src: &Utf8Path, _dst: &Utf8Path) -> Result<()> {
    Err(Error::Other(anyhow::anyhow!(
        "junctions are Windows-only; use symlink mode on Unix"
    )))
}

#[cfg(windows)]
fn create_file_symlink(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    std::os::windows::fs::symlink_file(src, dst)?;
    Ok(())
}

#[cfg(windows)]
fn create_dir_symlink(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)?;
    Ok(())
}

#[cfg(windows)]
fn create_junction(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    junction::create(src.as_std_path(), dst.as_std_path())?;
    Ok(())
}

/// Windows-only directory-link remover. Tries `junction::delete` first
/// (handles real junctions, may leave the empty entry which we then clean
/// up); falls back to `remove_dir` for directory symlinks and empty regular
/// dirs.
#[cfg(windows)]
fn remove_link_dir_windows(dst: &Utf8Path) -> Result<()> {
    if junction::delete(dst.as_std_path()).is_ok() {
        let _ = std::fs::remove_dir(dst);
        return Ok(());
    }
    std::fs::remove_dir(dst).map_err(|e| {
        Error::Other(anyhow::anyhow!(
            "unlink: {dst} not removed as a directory link: {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn utf8(p: std::path::PathBuf) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(p).unwrap()
    }

    #[test]
    fn auto_resolves_per_platform() {
        let f = resolve_file_mode(FileLinkMode::Auto);
        let d = resolve_dir_mode(DirLinkMode::Auto);
        if cfg!(windows) {
            assert_eq!(f, EffectiveFileMode::Hardlink);
            assert_eq!(d, EffectiveDirMode::Junction);
        } else {
            assert_eq!(f, EffectiveFileMode::Symlink);
            assert_eq!(d, EffectiveDirMode::Symlink);
        }
    }

    #[test]
    fn explicit_overrides_auto() {
        assert_eq!(
            resolve_file_mode(FileLinkMode::Symlink),
            EffectiveFileMode::Symlink
        );
        assert_eq!(
            resolve_dir_mode(DirLinkMode::Junction),
            EffectiveDirMode::Junction
        );
    }

    #[test]
    fn hardlink_file_and_unlink() {
        let tmp = TempDir::new().unwrap();
        let src = utf8(tmp.path().join("src.txt"));
        std::fs::write(&src, "hello").unwrap();

        let dst = utf8(tmp.path().join("nested/dst.txt"));
        link_file(&src, &dst, EffectiveFileMode::Hardlink).unwrap();

        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "hello");

        // Editing through dst affects src (hardlink → same inode).
        std::fs::write(&dst, "updated").unwrap();
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "updated");

        unlink(&dst).unwrap();
        assert!(!dst.exists());
        assert!(src.exists());
    }

    #[test]
    fn unlink_missing_is_noop() {
        let tmp = TempDir::new().unwrap();
        let dst = utf8(tmp.path().join("nonexistent"));
        unlink(&dst).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn junction_dir_and_unlink() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src_dir/sub")).unwrap();
        std::fs::write(tmp.path().join("src_dir/a.txt"), "A").unwrap();
        let src = utf8(std::fs::canonicalize(tmp.path().join("src_dir")).unwrap());

        let dst = utf8(tmp.path().join("nested/dst_dir"));
        link_dir(&src, &dst, EffectiveDirMode::Junction).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "A");
        assert!(dst.join("sub").is_dir());

        unlink(&dst).unwrap();
        assert!(!dst.exists());
        assert!(src.join("a.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_file_and_unlink() {
        let tmp = TempDir::new().unwrap();
        let src = utf8(tmp.path().join("src.txt"));
        std::fs::write(&src, "hello").unwrap();

        let dst = utf8(tmp.path().join("nested/dst.txt"));
        link_file(&src, &dst, EffectiveFileMode::Symlink).unwrap();

        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "hello");

        unlink(&dst).unwrap();
        assert!(!dst.exists());
        assert!(src.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_dir_and_unlink() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src_dir")).unwrap();
        std::fs::write(tmp.path().join("src_dir/a.txt"), "A").unwrap();
        let src = utf8(tmp.path().join("src_dir"));

        let dst = utf8(tmp.path().join("nested/dst_dir"));
        link_dir(&src, &dst, EffectiveDirMode::Symlink).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "A");

        unlink(&dst).unwrap();
        assert!(!dst.exists());
        assert!(src.join("a.txt").exists());
    }
}
