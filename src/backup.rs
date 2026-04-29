//! Backup creation under `$DOTFILES/.yui/backup/`.
//!
//! Path scheme: mirror the absolute target path (drive colon stripped on
//! Windows), then suffix the basename with a timestamp before the extension.
//!
//! ```text
//!   target  C:\Users\u\.config\foo\bar.yml
//!   backup  $DOTFILES/.yui/backup/C/Users/u/.config/foo/bar_20260429_143022123.yml
//! ```

use camino::{Utf8Path, Utf8PathBuf};

use crate::paths;
use crate::{Error, Result};

/// Format the current local time using a `jiff` strtime pattern.
pub fn current_timestamp(format: &str) -> Result<String> {
    let now = jiff::Zoned::now();
    let bdt = jiff::fmt::strtime::BrokenDownTime::from(&now);
    bdt.to_string(format)
        .map_err(|e| Error::Other(anyhow::anyhow!("timestamp format '{format}': {e}")))
}

pub fn backup_path(backup_root: &Utf8Path, abs_target: &Utf8Path, timestamp: &str) -> Utf8PathBuf {
    let mirrored = paths::mirror_into_backup(backup_root, abs_target);
    paths::append_timestamp(&mirrored, timestamp)
}

/// Copy `src` (a regular file) to `dest`, creating parent dirs as needed.
pub fn backup_file(src: &Utf8Path, dest: &Utf8Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dest)?;
    Ok(())
}

/// Recursively copy `src` (a directory tree) to `dest`. Symlinks within
/// the tree are skipped (we'd be copying their targets again redundantly,
/// and link semantics don't carry meaning in a backup).
pub fn backup_dir(src: &Utf8Path, dest: &Utf8Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    copy_dir_recursive(src, dest)
}

fn copy_dir_recursive(src: &Utf8Path, dest: &Utf8Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        let src_path = src.join(name);
        let dest_path = dest.join(name);
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else if ft.is_file() {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn backup_path_combines_mirror_and_timestamp() {
        let r = backup_path(
            Utf8Path::new("/dotfiles/.yui/backup"),
            Utf8Path::new("/home/u/.config/foo.yml"),
            "20260429_143022123",
        );
        assert_eq!(
            r,
            Utf8PathBuf::from("/dotfiles/.yui/backup/home/u/.config/foo_20260429_143022123.yml")
        );
    }

    #[test]
    fn current_timestamp_renders() {
        let s = current_timestamp("%Y%m%d_%H%M%S").unwrap();
        // Format: 8 digits underscore 6 digits.
        assert_eq!(s.len(), 15);
        assert!(s.chars().nth(8) == Some('_'));
    }

    #[test]
    fn backup_file_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let src = Utf8PathBuf::from_path_buf(tmp.path().join("input.txt")).unwrap();
        std::fs::write(&src, "hello").unwrap();
        let dest = Utf8PathBuf::from_path_buf(tmp.path().join("nested/dir/out.txt")).unwrap();
        backup_file(&src, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello");
    }

    #[test]
    fn backup_dir_copies_tree() {
        let tmp = TempDir::new().unwrap();
        let src = Utf8PathBuf::from_path_buf(tmp.path().join("src")).unwrap();
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "A").unwrap();
        std::fs::write(src.join("sub/b.txt"), "B").unwrap();

        let dest = Utf8PathBuf::from_path_buf(tmp.path().join("dest")).unwrap();
        backup_dir(&src, &dest).unwrap();

        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "A");
        assert_eq!(
            std::fs::read_to_string(dest.join("sub/b.txt")).unwrap(),
            "B"
        );
    }
}
