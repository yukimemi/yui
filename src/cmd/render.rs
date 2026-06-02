use super::*;
use crate::config;
use crate::render;
use crate::vars::YuiVars;
use anyhow::Result;
use camino::Utf8PathBuf;

pub fn render(source: Option<Utf8PathBuf>, check: bool, dry_run: bool) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;
    // --check is a stricter dry-run: never writes, exits non-zero on drift.
    let effective_dry_run = dry_run || check;
    let report = render::render_all(&source, &config, &yui, effective_dry_run)?;
    log_render_report(&report);
    // Stand-alone `yui render` has no secrets pipeline running
    // alongside, so the managed section here just covers `*.tera`
    // outputs. (Use `yui apply` if you need both rendered AND
    // decrypted siblings to land in the same write.)
    if !effective_dry_run && config.render.manage_gitignore {
        let managed = render::report_managed_paths(&report);
        render::write_managed_section(&source, &managed)?;
    }
    if check && report.has_drift() {
        anyhow::bail!("render drift detected ({} file(s))", report.diverged.len());
    }
    Ok(())
}
