use crate::paths;
use anyhow::{Context as _, Result};
use camino::{Utf8Path, Utf8PathBuf};

pub(crate) fn supports_color_stdout() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Strip the outer `{{ ... }}` Tera braces from a `when` expression for
/// display purposes (shorter line, easier to read at a glance).
pub(crate) fn strip_braces(expr: &str) -> String {
    let trimmed = expr.trim();
    if let Some(inner) = trimmed
        .strip_prefix("{{")
        .and_then(|s| s.strip_suffix("}}"))
    {
        inner.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

/// One side of a textual diff. `Binary` means the bytes weren't
/// valid UTF-8 (likely a binary file); the diff renderer surfaces
/// a one-liner instead of dumping bytes through `similar`.
/// Missing-file / permission errors collapse to `Text("")` so a
/// race during the walk doesn't bail the whole flow.
pub(crate) enum DiffSide {
    Text(String),
    Binary,
}

pub(crate) fn read_text_for_diff(p: &Utf8Path) -> DiffSide {
    match std::fs::read_to_string(p) {
        Ok(s) => DiffSide::Text(s),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => DiffSide::Binary,
        Err(_) => DiffSide::Text(String::new()),
    }
}

pub(crate) fn resolve_source(source: Option<Utf8PathBuf>) -> Result<Utf8PathBuf> {
    if let Some(s) = source {
        return absolutize(&s);
    }
    if let Ok(s) = std::env::var("YUI_SOURCE") {
        return absolutize(Utf8Path::new(&s));
    }
    let cwd = current_dir_utf8()?;
    for ancestor in cwd.ancestors() {
        if ancestor.join("config.toml").is_file() {
            return Ok(ancestor.to_path_buf());
        }
    }
    if let Some(home) = paths::home_dir() {
        for c in ["dotfiles", ".dotfiles", "src/dotfiles"] {
            let p = home.join(c);
            if p.join("config.toml").is_file() {
                return Ok(p);
            }
        }
    }
    anyhow::bail!("source repo not found (set --source / $YUI_SOURCE)")
}

pub(crate) fn absolutize(p: &Utf8Path) -> Result<Utf8PathBuf> {
    // Expand `~` first so callers can pass `--source ~/dotfiles` directly.
    let expanded = paths::expand_tilde(p.as_str());
    if expanded.is_absolute() {
        return Ok(crate::paths::normalize(&expanded));
    }
    let cwd = current_dir_utf8()?;
    Ok(crate::paths::normalize(&cwd.join(expanded)))
}

pub(crate) fn current_dir_utf8() -> Result<Utf8PathBuf> {
    let cwd = std::env::current_dir().context("getting cwd")?;
    Utf8PathBuf::from_path_buf(cwd).map_err(|p| anyhow::anyhow!("non-UTF8 cwd: {}", p.display()))
}

// Note: `home_dir()` lives in `paths.rs` so the tilde-expansion helper and
// `resolve_source` share one HOME/USERPROFILE lookup.
