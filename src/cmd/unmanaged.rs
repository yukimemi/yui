use super::*;
use crate::config::{self, IconsMode};
use crate::icons::Icons;
use crate::paths;
use crate::template;
use crate::vars::YuiVars;
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

/// `yui unmanaged [--icons MODE] [--no-color]` — list source files
/// that no `[[mount.entry]]` claims.
///
/// Useful for spotting orphans: files committed to the dotfiles
/// repo that yui never propagates anywhere. The walk goes through
/// `paths::source_walker`, which already honours nested
/// `.yuiignore` and skips `.yui/`. We additionally skip the repo's
/// own meta files (`config*.toml`, `.gitignore`, `.yuilink`,
/// `.yuiignore`, `*.tera` template sources) since "expected
/// unmanaged" entries would just bury the long tail.
pub fn unmanaged(
    source: Option<Utf8PathBuf>,
    icons_override: Option<IconsMode>,
    no_color: bool,
) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    let _icons = Icons::for_mode(icons_override.unwrap_or(config.ui.icons));
    let color = !no_color && supports_color_stdout();

    // Resolve every mount.src to an absolute path so a simple
    // `path.starts_with(&mount_src)` test can answer "claimed?".
    //
    //   - Iterate raw `config.mount.entry` (NOT `mount::resolve`)
    //     so a `when=false` mount still claims its files — surfacing
    //     them as "unmanaged" because they're inactive on this host
    //     would be confusing. (PR #53 review.)
    //   - Tera-render `entry.src` first so a templated path like
    //     `"private/{{ yui.host }}/home"` claims its files on
    //     this host rather than landing in `mount_srcs` as the
    //     literal raw string. (PR #56 review.)
    //   - `paths::resolve_mount_src` then applies tilde / absolute
    //     handling so private clones outside `$DOTFILES`
    //     participate too.
    let mut engine = template::Engine::new();
    let tera_ctx = template::template_context(&yui, &config.vars);
    let mount_srcs: Vec<Utf8PathBuf> = config
        .mount
        .entry
        .iter()
        .map(|e| -> Result<Utf8PathBuf> {
            let rendered = engine.render(e.src.as_str(), &tera_ctx)?;
            Ok(paths::resolve_mount_src(&source, rendered.trim()))
        })
        .collect::<Result<_>>()?;

    let mut items: Vec<Utf8PathBuf> = Vec::new();
    let walker = paths::source_walker(&source).build();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let std_path = entry.path();
        let path = match Utf8PathBuf::from_path_buf(std_path.to_path_buf()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Filter out the repo's own meta files. These are "managed
        // by yui itself" rather than "unmanaged orphans", so
        // surfacing them in the report is just noise.
        if is_repo_meta(&path, &source, &config.mount.marker_filename) {
            continue;
        }
        if mount_srcs.iter().any(|m| path.starts_with(m)) {
            continue;
        }
        items.push(path);
    }
    items.sort();

    if items.is_empty() {
        println!("  no unmanaged files under {source}");
        return Ok(());
    }

    print_unmanaged_table(&items, &source, color);
    println!();
    println!("  {} unmanaged file(s)", items.len());
    Ok(())
}

/// True for the dotfiles repo's own scaffold files — anything yui
/// itself reads or writes during its own operation. Surfacing
/// these in `yui unmanaged` would just bury the actual orphans.
///
/// Files keyed strictly by basename anywhere in the tree:
///   - `.yuilink` (mount marker)
///   - `.yuiignore` (yui's gitignore-style filter)
///   - `*.tera` (template sources)
///
/// Files keyed at the repo root only:
///   - `.gitignore` (yui manages the rendered-files section there;
///     a nested `home/.config/foo/.gitignore` is a user dotfile)
///   - `config.toml` / `config.local.toml` / `config.*.toml` /
///     `config.*.example.toml` (yui's own config layering;
///     a nested `home/.config/myapp/config.toml` is a user dotfile)
pub(crate) fn is_repo_meta(path: &Utf8Path, source: &Utf8Path, marker_filename: &str) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    if name.ends_with(".tera") {
        return true;
    }
    if name == marker_filename || name == ".yuiignore" {
        return true;
    }
    let parent = path.parent().unwrap_or(Utf8Path::new(""));
    let at_root = parent == source;
    if at_root && name == ".gitignore" {
        return true;
    }
    if at_root && (name == "config.toml" || name == "config.local.toml") {
        return true;
    }
    if at_root
        && name.starts_with("config.")
        && (name.ends_with(".toml") || name.ends_with(".example.toml"))
    {
        return true;
    }
    false
}

fn print_unmanaged_table(items: &[Utf8PathBuf], source: &Utf8Path, color: bool) {
    use owo_colors::OwoColorize as _;
    if color {
        println!("  {}", "PATH (relative to source)".dimmed());
    } else {
        println!("  PATH (relative to source)");
    }
    for p in items {
        let rel = p
            .strip_prefix(source)
            .map(Utf8PathBuf::from)
            .unwrap_or_else(|_| p.clone());
        if color {
            println!("  {}", rel.cyan());
        } else {
            println!("  {rel}");
        }
    }
}
