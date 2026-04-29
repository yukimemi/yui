//! Tera template rendering for `*.tera` files.
//!
//! Output goes to the **same directory** as the source `.tera` file (e.g.
//! `home/.gitconfig.tera` → `home/.gitconfig`). When `manage_gitignore` is
//! true, the rendered files are listed in a `# >>> yui rendered ... <<<`
//! managed section of `.gitignore`.

use camino::{Utf8Path, Utf8PathBuf};

use crate::Result;
use crate::config::Config;
use crate::vars::YuiVars;

#[derive(Debug, Default)]
pub struct RenderReport {
    pub written: Vec<Utf8PathBuf>,
    pub skipped_when_false: Vec<Utf8PathBuf>,
    /// Rendered counterpart already exists with diverged content (manual edit).
    pub diverged: Vec<Utf8PathBuf>,
}

/// Render every `*.tera` under the source tree, honoring per-file
/// `{# yui:when ... #}` headers and `[[render.rule]]` config entries.
pub fn render_all(_source: &Utf8Path, _config: &Config, _vars: &YuiVars) -> Result<RenderReport> {
    todo!("render::render_all")
}
