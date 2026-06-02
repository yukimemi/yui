use super::*;
use crate::link;
use anyhow::Result;
use camino::Utf8PathBuf;
use tracing::info;

pub fn unlink(source: Option<Utf8PathBuf>, paths_arg: Vec<Utf8PathBuf>) -> Result<()> {
    let _source = resolve_source(source)?;
    if paths_arg.is_empty() {
        anyhow::bail!("yui unlink: provide at least one target path");
    }
    for p in paths_arg {
        let abs = absolutize(&p)?;
        info!("unlink: {abs}");
        link::unlink(&abs)?;
    }
    Ok(())
}
