use super::*;
use anyhow::Result;
use camino::Utf8PathBuf;

pub fn link(source: Option<Utf8PathBuf>, dry_run: bool) -> Result<()> {
    // For now `link` and `apply` do the same thing (no render/absorb yet).
    apply(source, dry_run)
}
