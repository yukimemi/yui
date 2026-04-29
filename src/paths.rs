//! Path utilities for backup-mirroring and timestamp suffixing.

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};

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
}
