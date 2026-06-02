use super::*;
use crate::config::{self, Config, IconsMode, MountStrategy};
use crate::icons::Icons;
use crate::marker::{self, MarkerSpec};
use crate::mount;
use crate::render;
use crate::template;
use crate::vars::YuiVars;
use crate::{absorb, paths};
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use tera::Context as TeraContext;
use tracing::warn;

/// Show every src→dst pair's drift state against the current host.
///
/// Walks each `[[mount.entry]]`'s source tree, honoring `.yuilink`
/// markers (PassThrough = single dir-level link, Override = one or more
/// custom dsts), classifies each pair via [`crate::absorb::classify`],
/// and additionally surfaces any **render drift** — rendered files
/// whose content has diverged from what the matching `.tera` template
/// would produce now (i.e. the user edited the rendered file in place
/// without reflecting the change back into the template).
///
/// Exits non-zero (via `anyhow::bail!`) when anything diverges, so
/// `yui status && …` can gate workflows on a clean tree.
pub fn status(
    source: Option<Utf8PathBuf>,
    icons_override: Option<IconsMode>,
    no_color: bool,
) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    let mut engine = template::Engine::new();
    let tera_ctx = template::template_context(&yui, &config.vars);
    let mounts = mount::resolve(
        &source,
        &config.mount.entry,
        config.mount.default_strategy,
        &mut engine,
        &tera_ctx,
    )?;

    let icons_mode = icons_override.unwrap_or(config.ui.icons);
    let icons = Icons::for_mode(icons_mode);
    let color = !no_color && supports_color_stdout();

    let mut report: Vec<StatusItem> = Vec::new();

    // 1. Template drift — render in dry-run mode and surface anything
    //    whose rendered counterpart on disk no longer matches.
    let render_report = render::render_all(&source, &config, &yui, /* dry_run */ true)?;
    for entry in &render_report.diverged {
        // Show the `.tera` as src so it's clear which file the user
        // would edit to reflect a target-side change back into the
        // template.
        report.push(StatusItem {
            src: relative_for_display(&source, &entry.tera_path),
            dst: entry.rendered_path.clone(),
            state: StatusState::RenderDrift,
        });
    }

    // 2. Link drift — classify each src→dst pair under every mount.
    // Single nested-`.yuiignore` stack threaded across all mounts.
    // Seed the source-root layer so root rules apply from the start.
    let mut yuiignore = paths::YuiIgnoreStack::new();
    yuiignore.push_dir(&source)?;
    let walk_result = (|| -> Result<()> {
        for m in &mounts {
            let src_root = m.src.clone();
            if !src_root.is_dir() {
                warn!("mount src missing: {src_root}");
                continue;
            }
            classify_walk(
                &src_root,
                &m.dst,
                &config,
                m.strategy,
                &mut engine,
                &tera_ctx,
                &source,
                &mut yuiignore,
                &mut report,
            )?;
        }
        Ok(())
    })();
    yuiignore.pop_dir(&source);
    walk_result?;

    report.sort_by(|a, b| a.src.cmp(&b.src).then_with(|| a.dst.cmp(&b.dst)));

    print_status_table(&report, icons, color);

    let drift = report.iter().filter(|r| !r.state.is_in_sync()).count();

    println!();
    let total = report.len();
    let in_sync = total - drift;
    if drift == 0 {
        println!("  {total} entries · all in sync");
        Ok(())
    } else {
        println!("  {total} entries · {in_sync} in sync · {drift} diverged");
        anyhow::bail!("status: {drift} entries diverged from source")
    }
}

#[derive(Debug)]
pub(crate) struct StatusItem {
    /// Path under the source tree (display only).
    pub(crate) src: Utf8PathBuf,
    /// Resolved target path (or rendered output path for `RenderDrift`).
    pub(crate) dst: Utf8PathBuf,
    pub(crate) state: StatusState,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum StatusState {
    Link(absorb::AbsorbDecision),
    /// Rendered output diverges from current `.tera` template — user
    /// edited the rendered file directly without updating the template.
    RenderDrift,
}

impl StatusState {
    fn is_in_sync(self) -> bool {
        matches!(self, Self::Link(absorb::AbsorbDecision::InSync))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn classify_walk(
    src_dir: &Utf8Path,
    dst_dir: &Utf8Path,
    config: &Config,
    strategy: MountStrategy,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
    source_root: &Utf8Path,
    yuiignore: &mut paths::YuiIgnoreStack,
    report: &mut Vec<StatusItem>,
) -> Result<()> {
    classify_walk_inner(
        src_dir,
        dst_dir,
        config,
        strategy,
        engine,
        tera_ctx,
        source_root,
        yuiignore,
        report,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn classify_walk_inner(
    src_dir: &Utf8Path,
    dst_dir: &Utf8Path,
    config: &Config,
    strategy: MountStrategy,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
    source_root: &Utf8Path,
    yuiignore: &mut paths::YuiIgnoreStack,
    report: &mut Vec<StatusItem>,
    parent_covered: bool,
) -> Result<()> {
    if yuiignore.is_ignored(src_dir, /* is_dir */ true) {
        return Ok(());
    }
    // Layer this dir's .yuiignore (if any) on top before we recurse;
    // pop on exit so siblings don't see our subtree's rules.
    yuiignore.push_dir(src_dir)?;
    let result = classify_walk_inner_body(
        src_dir,
        dst_dir,
        config,
        strategy,
        engine,
        tera_ctx,
        source_root,
        yuiignore,
        report,
        parent_covered,
    );
    yuiignore.pop_dir(src_dir);
    result
}

#[allow(clippy::too_many_arguments)]
fn classify_walk_inner_body(
    src_dir: &Utf8Path,
    dst_dir: &Utf8Path,
    config: &Config,
    strategy: MountStrategy,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
    source_root: &Utf8Path,
    yuiignore: &mut paths::YuiIgnoreStack,
    report: &mut Vec<StatusItem>,
    parent_covered: bool,
) -> Result<()> {
    let marker_filename = &config.mount.marker_filename;
    let mut covered = parent_covered;

    if strategy == MountStrategy::Marker {
        match marker::read_spec(src_dir, marker_filename)? {
            None => {}
            Some(MarkerSpec::PassThrough) => {
                let decision = absorb::classify(src_dir, dst_dir)?;
                report.push(StatusItem {
                    src: relative_for_display(source_root, src_dir),
                    dst: dst_dir.to_path_buf(),
                    state: StatusState::Link(decision),
                });
                covered = true;
            }
            Some(MarkerSpec::Explicit { links }) => {
                let mut emitted_dir_link = false;
                for link in &links {
                    if let Some(when) = &link.when {
                        if !template::eval_truthy(when, engine, tera_ctx)? {
                            continue;
                        }
                    }
                    let dst_str = engine.render(&link.dst, tera_ctx)?;
                    let dst = paths::expand_tilde(dst_str.trim());
                    if let Some(filename) = &link.src {
                        let file_src = src_dir.join(filename);
                        if !file_src.is_file() {
                            anyhow::bail!(
                                "marker at {src_dir}: [[link]] src={filename:?} \
                                 not found"
                            );
                        }
                        let decision = absorb::classify(&file_src, &dst)?;
                        report.push(StatusItem {
                            src: relative_for_display(source_root, &file_src),
                            dst,
                            state: StatusState::Link(decision),
                        });
                    } else {
                        let decision = absorb::classify(src_dir, &dst)?;
                        report.push(StatusItem {
                            src: relative_for_display(source_root, src_dir),
                            dst,
                            state: StatusState::Link(decision),
                        });
                        emitted_dir_link = true;
                    }
                }
                if emitted_dir_link {
                    covered = true;
                }
            }
        }
    }

    for entry in std::fs::read_dir(src_dir)? {
        let entry = entry?;
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        if name == marker_filename || name.ends_with(".tera") {
            continue;
        }
        let src_path = src_dir.join(name);
        let dst_path = dst_dir.join(name);
        let ft = entry.file_type()?;
        if yuiignore.is_ignored(&src_path, ft.is_dir()) {
            continue;
        }
        if ft.is_dir() {
            classify_walk_inner(
                &src_path,
                &dst_path,
                config,
                strategy,
                engine,
                tera_ctx,
                source_root,
                yuiignore,
                report,
                covered,
            )?;
        } else if ft.is_file() && !covered {
            let decision = absorb::classify(&src_path, &dst_path)?;
            report.push(StatusItem {
                src: relative_for_display(source_root, &src_path),
                dst: dst_path,
                state: StatusState::Link(decision),
            });
        }
    }
    Ok(())
}

fn relative_for_display(source_root: &Utf8Path, p: &Utf8Path) -> Utf8PathBuf {
    p.strip_prefix(source_root)
        .map(Utf8PathBuf::from)
        .unwrap_or_else(|_| p.to_path_buf())
}

fn print_status_table(items: &[StatusItem], icons: Icons, color: bool) {
    let src_w = items
        .iter()
        .map(|i| i.src.as_str().chars().count())
        .max()
        .unwrap_or(0)
        .max("SRC".len());
    let dst_w = items
        .iter()
        .map(|i| i.dst.as_str().chars().count())
        .max()
        .unwrap_or(0)
        .max("DST".len());
    // STATE column = icon (1ch) + space + longest label
    let state_label_w = items
        .iter()
        .map(|i| state_label(i.state).len())
        .max()
        .unwrap_or(0)
        .max("STATE".len() - 2); // "STATE" header takes 5 chars; the icon prefix accounts for 2
    let state_w = state_label_w + 2; // " " + label

    print_status_header(state_w, src_w, dst_w, color);
    let sep = render_status_separator(icons.sep, state_w, src_w, dst_w, icons.arrow);
    if color {
        use owo_colors::OwoColorize as _;
        println!("{}", sep.dimmed());
    } else {
        println!("{sep}");
    }
    for item in items {
        print_status_row(item, icons, state_w, src_w, dst_w, color);
    }
}

fn state_label(s: StatusState) -> &'static str {
    use absorb::AbsorbDecision::*;
    match s {
        StatusState::Link(InSync) => "in-sync",
        StatusState::Link(RelinkOnly) => "relink",
        StatusState::Link(AutoAbsorb) => "drift (auto)",
        StatusState::Link(NeedsConfirm) => "drift (anomaly)",
        StatusState::Link(Restore) => "missing",
        StatusState::RenderDrift => "render drift",
    }
}

fn state_icon(s: StatusState, icons: Icons) -> &'static str {
    use absorb::AbsorbDecision::*;
    match s {
        StatusState::Link(InSync) => icons.ok,
        StatusState::Link(RelinkOnly) => icons.warn,
        StatusState::Link(AutoAbsorb) => icons.warn,
        StatusState::Link(NeedsConfirm) => icons.error,
        StatusState::Link(Restore) => icons.info,
        StatusState::RenderDrift => icons.error,
    }
}

fn print_status_header(state_w: usize, src_w: usize, dst_w: usize, color: bool) {
    use owo_colors::OwoColorize as _;
    // STATE is the only column with data above; "WHEN" intentionally omitted
    // since status only shows mounts that are already active on this host.
    let line = format!(
        "  {:<state_w$}  {:<src_w$}     {:<dst_w$}",
        "STATE", "SRC", "DST"
    );
    if color {
        println!("{}", line.bold());
    } else {
        println!("{line}");
    }
}

fn render_status_separator(
    sep_ch: char,
    state_w: usize,
    src_w: usize,
    dst_w: usize,
    arrow: &str,
) -> String {
    let bar = |n: usize| sep_ch.to_string().repeat(n);
    format!(
        "  {}  {}  {}  {}",
        bar(state_w),
        bar(src_w),
        bar(arrow.chars().count()),
        bar(dst_w)
    )
}

fn print_status_row(
    item: &StatusItem,
    icons: Icons,
    state_w: usize,
    src_w: usize,
    dst_w: usize,
    color: bool,
) {
    use owo_colors::OwoColorize as _;
    let icon = state_icon(item.state, icons);
    let label = state_label(item.state);
    let state_text = format!("{icon} {label}");
    let src_display = item.src.as_str().replace('\\', "/");
    let dst_display = item.dst.as_str().replace('\\', "/");
    let arrow = icons.arrow;

    let cell_state = format!("{:<state_w$}", state_text);
    let cell_src = format!("{:<src_w$}", src_display);
    let cell_dst = format!("{:<dst_w$}", dst_display);

    if !color {
        println!("  {cell_state}  {cell_src}  {arrow}  {cell_dst}");
        return;
    }

    use absorb::AbsorbDecision::*;
    let state_colored = match item.state {
        StatusState::Link(InSync) => cell_state.green().to_string(),
        StatusState::Link(RelinkOnly) | StatusState::Link(AutoAbsorb) => {
            cell_state.yellow().to_string()
        }
        StatusState::Link(NeedsConfirm) => cell_state.red().to_string(),
        StatusState::Link(Restore) => cell_state.cyan().to_string(),
        StatusState::RenderDrift => cell_state.red().to_string(),
    };
    let src_colored = cell_src.cyan().to_string();
    let arrow_colored = arrow.dimmed().to_string();
    let dst_colored = cell_dst.dimmed().to_string();
    println!("  {state_colored}  {src_colored}  {arrow_colored}  {dst_colored}");
}
