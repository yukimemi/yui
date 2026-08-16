use super::*;
use crate::config::{self, Config, IconsMode};
use crate::icons::Icons;
use crate::links::{LinkMode, LinkPlan};
use crate::paths;
use crate::template;
use crate::vars::YuiVars;
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use std::fmt::Write as _;

/// Show the resolved src→dst mappings for the current source repo.
///
/// By default only entries whose `when` matches the current host are shown
/// (`active`). With `--all`, inactive entries are included with a dim row
/// and the `when` condition that excluded them.
pub fn list(
    source: Option<Utf8PathBuf>,
    all: bool,
    icons_override: Option<IconsMode>,
    no_color: bool,
) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    let icons_mode = icons_override.unwrap_or(config.ui.icons);
    let icons = Icons::for_mode(icons_mode);
    let color = !no_color && supports_color_stdout();

    let items = collect_list_items(&source, &config, &yui)?;
    let displayed: Vec<&ListItem> = if all {
        items.iter().collect()
    } else {
        items.iter().filter(|i| i.active).collect()
    };

    print_list_table(&displayed, icons, color);

    let total = items.len();
    let active = items.iter().filter(|i| i.active).count();
    let inactive = total - active;
    println!();
    if all {
        println!("  {total} entries · {active} active · {inactive} inactive");
    } else {
        println!(
            "  {} of {} entries shown ({} inactive hidden — use --all)",
            active, total, inactive
        );
    }
    Ok(())
}

#[derive(Debug)]
struct ListItem {
    src: Utf8PathBuf,
    dst: String,
    when: Option<String>,
    active: bool,
    /// Per-entry `mode` override, if the entry declares one. `None`
    /// means "whatever `[mount]` says", which is the common case — the
    /// column only appears when something actually overrides.
    mode: Option<&'static str>,
}

fn collect_list_items(source: &Utf8Path, config: &Config, yui: &YuiVars) -> Result<Vec<ListItem>> {
    let mut engine = template::Engine::new();
    let tera_ctx = template::template_context(yui, &config.vars);
    let mut items = Vec::new();

    // 1. config.toml [[mount.entry]] entries
    for entry in &config.mount.entry {
        let active = match &entry.when {
            None => true,
            Some(w) => template::eval_truthy(w, &mut engine, &tera_ctx)?,
        };
        let dst = engine
            .render(&entry.dst, &tera_ctx)
            .map(|s| paths::expand_tilde(s.trim()).to_string())
            .unwrap_or_else(|_| entry.dst.clone());
        items.push(ListItem {
            src: entry.src.clone(),
            dst,
            when: entry.when.clone(),
            active,
            mode: None,
        });
    }

    // 2. `[[link]]` declarations — central entries from config.toml plus
    //    every `.yuilink` under source. Both normalize to the same
    //    shape, so one loop covers both.
    let plan = LinkPlan::from_config(source, &config.link)?;
    let marker_filename = &config.mount.marker_filename;
    for dir in plan.declared_dirs(source, marker_filename) {
        let spec = plan.dir_spec(&dir, marker_filename, true)?;
        // PassThrough markers are already implied by the mount entry.
        if spec.links.is_empty() {
            continue;
        }
        let rel = dir
            .strip_prefix(source)
            .map(Utf8PathBuf::from)
            .unwrap_or_else(|_| dir.clone());
        for link in &spec.links {
            let active = match &link.when {
                None => true,
                Some(w) => template::eval_truthy(w, &mut engine, &tera_ctx)?,
            };
            let dst = engine
                .render(&link.dst, &tera_ctx)
                .map(|s| paths::expand_tilde(s.trim()).to_string())
                .unwrap_or_else(|_| link.dst.clone());
            // File-scoped entry (`[[link]] src = "<path>"`) targets a
            // single file inside the dir; show that file path instead of
            // the bare dir so `yui list` makes the scope obvious at a
            // glance.
            let src_display = match &link.rel {
                Some(r) => rel.join(r),
                None => rel.clone(),
            };
            items.push(ListItem {
                src: src_display,
                dst,
                when: link.when.clone(),
                active,
                mode: link.mode.map(LinkMode::as_str),
            });
        }
    }

    items.sort_by(|a, b| a.src.cmp(&b.src).then_with(|| a.dst.cmp(&b.dst)));
    Ok(items)
}

fn print_list_table(items: &[&ListItem], icons: Icons, color: bool) {
    let src_w = items
        .iter()
        .map(|i| i.src.as_str().chars().count())
        .max()
        .unwrap_or(0)
        .max("SRC".len());
    let dst_w = items
        .iter()
        .map(|i| i.dst.chars().count())
        .max()
        .unwrap_or(0)
        .max("DST".len());
    // The MODE column only exists when something overrides `[mount]`.
    // Most repos never do, and an always-empty column is just noise.
    let mode_w = items
        .iter()
        .filter_map(|i| i.mode)
        .map(|m| m.chars().count())
        .max()
        .map(|w| w.max("MODE".len()));

    let status_w = "STATUS".len();
    let arrow_w = icons.arrow.chars().count();

    // Header
    print_header(status_w, src_w, arrow_w, dst_w, mode_w, color);

    // Separator
    let sep = render_separator(icons.sep, status_w, src_w, arrow_w, dst_w, mode_w);
    if color {
        use owo_colors::OwoColorize as _;
        println!("{}", sep.dimmed());
    } else {
        println!("{sep}");
    }

    // Rows
    for item in items {
        print_row(item, icons, status_w, src_w, arrow_w, dst_w, mode_w, color);
    }
}

fn print_header(
    status_w: usize,
    src_w: usize,
    arrow_w: usize,
    dst_w: usize,
    mode_w: Option<usize>,
    color: bool,
) {
    use owo_colors::OwoColorize as _;
    let mut line = String::new();
    let _ = write!(
        &mut line,
        "  {:<status_w$}  {:<src_w$}  {:<arrow_w$}  {:<dst_w$}",
        "STATUS", "SRC", "", "DST"
    );
    if let Some(w) = mode_w {
        let _ = write!(&mut line, "  {:<w$}", "MODE");
    }
    let _ = write!(&mut line, "  WHEN");
    if color {
        println!("{}", line.bold());
    } else {
        println!("{line}");
    }
}

fn render_separator(
    sep_ch: char,
    status_w: usize,
    src_w: usize,
    arrow_w: usize,
    dst_w: usize,
    mode_w: Option<usize>,
) -> String {
    let bar = |n: usize| sep_ch.to_string().repeat(n);
    let mut out = format!(
        "  {}  {}  {}  {}",
        bar(status_w),
        bar(src_w),
        bar(arrow_w),
        bar(dst_w),
    );
    if let Some(w) = mode_w {
        let _ = write!(&mut out, "  {}", bar(w));
    }
    let _ = write!(&mut out, "  {}", bar("WHEN".len()));
    out
}

#[allow(clippy::too_many_arguments)]
fn print_row(
    item: &ListItem,
    icons: Icons,
    status_w: usize,
    src_w: usize,
    arrow_w: usize,
    dst_w: usize,
    mode_w: Option<usize>,
    color: bool,
) {
    use owo_colors::OwoColorize as _;
    let status = if item.active {
        icons.active
    } else {
        icons.inactive
    };
    let when_str = item
        .when
        .as_deref()
        .map(strip_braces)
        .unwrap_or_else(|| "(always)".to_string());

    // Normalize backslashes to forward slashes for cross-platform display.
    let src_display = item.src.as_str().replace('\\', "/");
    let src = src_display.as_str();
    let dst = &item.dst;
    let arrow = icons.arrow;

    // Pad each cell to its column width FIRST, then apply color. Doing it
    // the other way round lets ANSI escape codes count as printable chars
    // in `format!("{:<w$}")`, which silently breaks alignment when colors
    // are enabled (caught in PR #11 review).
    let cell_status = format!("{:<status_w$}", status);
    let cell_src = format!("{:<src_w$}", src);
    let cell_arrow = format!("{:<arrow_w$}", arrow);
    let cell_dst = format!("{:<dst_w$}", dst);
    // Entries without an override show `-`: the column exists because
    // *some* entry overrides, and a blank cell would read as missing
    // data rather than "the default applies".
    let cell_mode = mode_w.map(|w| format!("{:<w$}", item.mode.unwrap_or("-")));

    if !color {
        let mut line = format!("  {cell_status}  {cell_src}  {cell_arrow}  {cell_dst}");
        if let Some(cell) = &cell_mode {
            let _ = write!(&mut line, "  {cell}");
        }
        let _ = write!(&mut line, "  {when_str}");
        println!("{line}");
        return;
    }

    let mut line = if item.active {
        format!(
            "  {}  {}  {}  {}",
            cell_status.green(),
            cell_src.cyan(),
            cell_arrow.dimmed(),
            cell_dst.green(),
        )
    } else {
        format!(
            "  {}  {}  {}  {}",
            cell_status.red().dimmed(),
            cell_src.dimmed(),
            cell_arrow.dimmed(),
            cell_dst.dimmed(),
        )
    };
    if let Some(cell) = &cell_mode {
        let _ = write!(&mut line, "  {}", cell.yellow());
    }
    let _ = write!(&mut line, "  {}", when_str.dimmed());
    println!("{line}");
}
