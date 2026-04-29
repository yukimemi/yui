//! `yui status` — show drift across all mounts:
//!   - link-broken     (target became a separate inode from source)
//!   - replaced        (junction/symlink replaced by a regular file/dir)
//!   - rendered-drift  (rendered file edited in-place, template not updated)
//!   - missing         (target gone but source exists)

use crate::Result;

pub fn run() -> Result<()> {
    todo!("status::run")
}
