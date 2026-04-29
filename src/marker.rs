//! `.yuilink` marker file detection.
//!
//! Under `marker` strategy, a directory containing the configured marker file
//! (default `.yuilink`) is the link point — `yui` creates a single
//! junction/symlink for that directory and stops recursing.

use camino::Utf8Path;

pub fn is_marker_dir(dir: &Utf8Path, marker_filename: &str) -> bool {
    dir.join(marker_filename).is_file()
}
