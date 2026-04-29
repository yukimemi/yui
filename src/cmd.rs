//! Command implementations.
//!
//! Each `Command` variant in `cli.rs` calls one of these. Currently
//! implemented: `apply`, `init`, `doctor`. The rest are `todo!()`.

use std::fmt::Write as _;

use anyhow::{Context as _, Result};
use camino::{Utf8Path, Utf8PathBuf};
use tera::Context as TeraContext;
use tracing::{info, warn};

use crate::config::{self, Config, HookPhase, IconsMode, MountStrategy};
use crate::hook::{self, HookOutcome};
use crate::icons::Icons;
use crate::link::{self, EffectiveDirMode, EffectiveFileMode, resolve_dir_mode, resolve_file_mode};
use crate::marker::{self, MarkerSpec};
use crate::mount::{self, ResolvedMount};
use crate::render::{self, RenderReport};
use crate::template;
use crate::vars::YuiVars;
use crate::{absorb, backup, paths};

// NOTE: `owo_colors::OwoColorize` is intentionally NOT imported at module
// scope — its blanket impl shadows inherent methods of unrelated types
// (e.g. `ignore::WalkBuilder::hidden(bool)` collides with
// `OwoColorize::hidden(&self)`). Each print function imports the trait
// locally with `use owo_colors::OwoColorize as _;`.

pub fn init(source: Option<Utf8PathBuf>, _git_hooks: bool) -> Result<()> {
    let dir = match source {
        Some(s) => absolutize(&s)?,
        None => current_dir_utf8()?,
    };
    std::fs::create_dir_all(&dir)?;
    let config_path = dir.join("config.toml");
    if config_path.exists() {
        anyhow::bail!("config.toml already exists at {config_path}");
    }
    std::fs::write(&config_path, SKELETON_CONFIG)?;
    let gitignore_path = dir.join(".gitignore");
    if !gitignore_path.exists() {
        std::fs::write(&gitignore_path, SKELETON_GITIGNORE)?;
    }
    info!("initialized yui source repo at {dir}");
    info!("created: {config_path}");
    info!("next: edit config.toml, then run `yui apply`");
    Ok(())
}

pub fn apply(source: Option<Utf8PathBuf>, dry_run: bool) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;
    // Load `.yuiignore` once and thread through render + walk so the
    // matcher isn't re-built per-flow.
    let yuiignore = paths::load_yuiignore(&source)?;

    let mut engine = template::Engine::new();
    let tera_ctx = template::template_context(&yui, &config.vars);

    // 0. Pre-apply hooks (before render / link). Bail on hook failure so
    //    apply doesn't proceed past a broken bootstrap.
    hook::run_phase(
        &config,
        &source,
        &yui,
        &mut engine,
        &tera_ctx,
        HookPhase::Pre,
        dry_run,
    )?;

    // 1. Render templates first so the link walk picks up rendered files.
    let render_report = render::render_all(&source, &config, &yui, &yuiignore, dry_run)?;
    log_render_report(&render_report);
    if render_report.has_drift() {
        anyhow::bail!(
            "render drift detected ({} file(s)); reflect target edits back into the .tera before re-running apply",
            render_report.diverged.len()
        );
    }

    // 2. Resolve mounts and link.
    let mounts = mount::resolve(
        &config.mount.entry,
        config.mount.default_strategy,
        &mut engine,
        &tera_ctx,
    )?;

    let backup_root = source.join(&config.backup.dir);
    let ctx = ApplyCtx {
        config: &config,
        source: &source,
        yuiignore: &yuiignore,
        file_mode: resolve_file_mode(config.link.file_mode),
        dir_mode: resolve_dir_mode(config.link.dir_mode),
        backup_root: &backup_root,
        dry_run,
    };

    info!("source: {source}");
    info!("modes: file={:?} dir={:?}", ctx.file_mode, ctx.dir_mode);
    if dry_run {
        info!("dry-run: nothing will be written");
    }

    for m in &mounts {
        info!("mount: {} → {}", m.src, m.dst);
        process_mount(&source, m, &ctx, &mut engine, &tera_ctx)?;
    }

    // 3. Post-apply hooks (after every link is in place).
    hook::run_phase(
        &config,
        &source,
        &yui,
        &mut engine,
        &tera_ctx,
        HookPhase::Post,
        dry_run,
    )?;
    Ok(())
}

fn log_render_report(r: &RenderReport) {
    if !r.written.is_empty() {
        info!("rendered {} new file(s)", r.written.len());
    }
    if !r.unchanged.is_empty() {
        info!("rendered {} file(s) unchanged", r.unchanged.len());
    }
    if !r.skipped_when_false.is_empty() {
        info!(
            "skipped {} template(s) (when=false)",
            r.skipped_when_false.len()
        );
    }
    for d in &r.diverged {
        warn!("rendered file diverged from template: {d}");
    }
}

/// Bundle of immutable settings threaded through the apply walk.
struct ApplyCtx<'a> {
    config: &'a Config,
    /// Source repo root — needed for git-clean checks during absorb and
    /// for resolving paths inside `is_ignored` against `.yuiignore`.
    source: &'a Utf8Path,
    /// Patterns from `$source/.yuiignore`. Empty matcher when the file
    /// is absent.
    yuiignore: &'a ignore::gitignore::Gitignore,
    file_mode: EffectiveFileMode,
    dir_mode: EffectiveDirMode,
    backup_root: &'a Utf8Path,
    dry_run: bool,
}

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
}

fn collect_list_items(source: &Utf8Path, config: &Config, yui: &YuiVars) -> Result<Vec<ListItem>> {
    let mut engine = template::Engine::new();
    let tera_ctx = template::template_context(yui, &config.vars);
    let yuiignore = paths::load_yuiignore(source)?;
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
        });
    }

    // 2. .yuilink overrides under source
    let walker = paths::source_walker(source).build();
    let marker_filename = &config.mount.marker_filename;
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if entry.path().file_name().and_then(|n| n.to_str()) != Some(marker_filename.as_str()) {
            continue;
        }
        let dir = match entry.path().parent() {
            Some(d) => d,
            None => continue,
        };
        let dir_utf8 = match Utf8PathBuf::from_path_buf(dir.to_path_buf()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        // .yuiignore filter — markers inside ignored subtrees are skipped.
        if paths::is_ignored(&yuiignore, source, &dir_utf8, true) {
            continue;
        }
        let spec = match marker::read_spec(&dir_utf8, marker_filename)? {
            Some(s) => s,
            None => continue,
        };
        let MarkerSpec::Override { links } = spec else {
            continue; // PassThrough markers are already implied by mount entry
        };
        let rel = dir_utf8
            .strip_prefix(source)
            .map(Utf8PathBuf::from)
            .unwrap_or(dir_utf8);
        for link in &links {
            let active = match &link.when {
                None => true,
                Some(w) => template::eval_truthy(w, &mut engine, &tera_ctx)?,
            };
            let dst = engine
                .render(&link.dst, &tera_ctx)
                .map(|s| paths::expand_tilde(s.trim()).to_string())
                .unwrap_or_else(|_| link.dst.clone());
            items.push(ListItem {
                src: rel.clone(),
                dst,
                when: link.when.clone(),
                active,
            });
        }
    }

    items.sort_by(|a, b| a.src.cmp(&b.src).then_with(|| a.dst.cmp(&b.dst)));
    Ok(items)
}

fn supports_color_stdout() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
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

    let status_w = "STATUS".len();
    let arrow_w = icons.arrow.chars().count();

    // Header
    print_header(status_w, src_w, arrow_w, dst_w, color);

    // Separator
    let sep = render_separator(icons.sep, status_w, src_w, arrow_w, dst_w);
    if color {
        use owo_colors::OwoColorize as _;
        println!("{}", sep.dimmed());
    } else {
        println!("{sep}");
    }

    // Rows
    for item in items {
        print_row(item, icons, status_w, src_w, arrow_w, dst_w, color);
    }
}

fn print_header(status_w: usize, src_w: usize, arrow_w: usize, dst_w: usize, color: bool) {
    use owo_colors::OwoColorize as _;
    let mut line = String::new();
    let _ = write!(
        &mut line,
        "  {:<status_w$}  {:<src_w$}  {:<arrow_w$}  {:<dst_w$}  WHEN",
        "STATUS", "SRC", "", "DST"
    );
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
) -> String {
    let bar = |n: usize| sep_ch.to_string().repeat(n);
    format!(
        "  {}  {}  {}  {}  {}",
        bar(status_w),
        bar(src_w),
        bar(arrow_w),
        bar(dst_w),
        bar("WHEN".len())
    )
}

fn print_row(
    item: &ListItem,
    icons: Icons,
    status_w: usize,
    src_w: usize,
    arrow_w: usize,
    dst_w: usize,
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

    if !color {
        println!("  {cell_status}  {cell_src}  {cell_arrow}  {cell_dst}  {when_str}");
        return;
    }

    if item.active {
        println!(
            "  {}  {}  {}  {}  {}",
            cell_status.green(),
            cell_src.cyan(),
            cell_arrow.dimmed(),
            cell_dst.green(),
            when_str.dimmed()
        );
    } else {
        println!(
            "  {}  {}  {}  {}  {}",
            cell_status.red().dimmed(),
            cell_src.dimmed(),
            cell_arrow.dimmed(),
            cell_dst.dimmed(),
            when_str.dimmed()
        );
    }
}

/// Strip the outer `{{ ... }}` Tera braces from a `when` expression for
/// display purposes (shorter line, easier to read at a glance).
fn strip_braces(expr: &str) -> String {
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

pub fn render(source: Option<Utf8PathBuf>, check: bool, dry_run: bool) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;
    let yuiignore = paths::load_yuiignore(&source)?;
    // --check is a stricter dry-run: never writes, exits non-zero on drift.
    let report = render::render_all(&source, &config, &yui, &yuiignore, dry_run || check)?;
    log_render_report(&report);
    if check && report.has_drift() {
        anyhow::bail!("render drift detected ({} file(s))", report.diverged.len());
    }
    Ok(())
}

pub fn link(source: Option<Utf8PathBuf>, dry_run: bool) -> Result<()> {
    // For now `link` and `apply` do the same thing (no render/absorb yet).
    apply(source, dry_run)
}

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
        &config.mount.entry,
        config.mount.default_strategy,
        &mut engine,
        &tera_ctx,
    )?;

    let icons_mode = icons_override.unwrap_or(config.ui.icons);
    let icons = Icons::for_mode(icons_mode);
    let color = !no_color && supports_color_stdout();

    let mut report: Vec<StatusItem> = Vec::new();
    // Load `.yuiignore` once and reuse for both render-drift detection
    // and the link-drift walk below.
    let yuiignore = paths::load_yuiignore(&source)?;

    // 1. Template drift — render in dry-run mode and surface anything
    //    whose rendered counterpart on disk no longer matches.
    let render_report =
        render::render_all(&source, &config, &yui, &yuiignore, /* dry_run */ true)?;
    for rendered in &render_report.diverged {
        // `diverged` holds the rendered path; the template lives at
        // `<rendered>.tera`. Show the .tera as src so it's clear which
        // file the user needs to update.
        let tera_path = Utf8PathBuf::from(format!("{rendered}.tera"));
        report.push(StatusItem {
            src: relative_for_display(&source, &tera_path),
            dst: rendered.clone(),
            state: StatusState::RenderDrift,
        });
    }

    // 2. Link drift — classify each src→dst pair under every mount.
    for m in &mounts {
        let src_root = source.join(&m.src);
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
            &yuiignore,
            &mut report,
        )?;
    }

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
struct StatusItem {
    /// Path under the source tree (display only).
    src: Utf8PathBuf,
    /// Resolved target path (or rendered output path for `RenderDrift`).
    dst: Utf8PathBuf,
    state: StatusState,
}

#[derive(Debug, Clone, Copy)]
enum StatusState {
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
fn classify_walk(
    src_dir: &Utf8Path,
    dst_dir: &Utf8Path,
    config: &Config,
    strategy: MountStrategy,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
    source_root: &Utf8Path,
    yuiignore: &ignore::gitignore::Gitignore,
    report: &mut Vec<StatusItem>,
) -> Result<()> {
    if paths::is_ignored(yuiignore, source_root, src_dir, /* is_dir */ true) {
        return Ok(());
    }

    let marker_filename = &config.mount.marker_filename;

    if strategy == MountStrategy::Marker {
        match marker::read_spec(src_dir, marker_filename)? {
            None => {} // no marker — fall through to recursive walk
            Some(MarkerSpec::PassThrough) => {
                let decision = absorb::classify(src_dir, dst_dir)?;
                report.push(StatusItem {
                    src: relative_for_display(source_root, src_dir),
                    dst: dst_dir.to_path_buf(),
                    state: StatusState::Link(decision),
                });
                return Ok(());
            }
            Some(MarkerSpec::Override { links }) => {
                for link in &links {
                    if let Some(when) = &link.when {
                        if !template::eval_truthy(when, engine, tera_ctx)? {
                            continue;
                        }
                    }
                    let dst_str = engine.render(&link.dst, tera_ctx)?;
                    let dst = paths::expand_tilde(dst_str.trim());
                    let decision = absorb::classify(src_dir, &dst)?;
                    report.push(StatusItem {
                        src: relative_for_display(source_root, src_dir),
                        dst,
                        state: StatusState::Link(decision),
                    });
                }
                return Ok(());
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
        if paths::is_ignored(yuiignore, source_root, &src_path, ft.is_dir()) {
            continue;
        }
        if ft.is_dir() {
            classify_walk(
                &src_path,
                &dst_path,
                config,
                strategy,
                engine,
                tera_ctx,
                source_root,
                yuiignore,
                report,
            )?;
        } else if ft.is_file() {
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

/// Manually absorb a single target file back into source.
///
/// Used when `apply` has skipped an anomaly (`[absorb] on_anomaly = "skip"`
/// or non-TTY ask) but the user has decided that target is right. Bypasses
/// policy + git-clean checks: this is an explicit user request.
///
/// Walks `[[mount.entry]]` and `.yuilink` overrides to find which source
/// path "owns" the given target. Errors loudly if no mount claims it.
pub fn absorb(source: Option<Utf8PathBuf>, target: Utf8PathBuf, dry_run: bool) -> Result<()> {
    let source = resolve_source(source)?;
    let target = absolutize(&target)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    let mut engine = template::Engine::new();
    let tera_ctx = template::template_context(&yui, &config.vars);
    // Single load — the matcher is shared with both find_source_for_target
    // and the eventual ApplyCtx below.
    let yuiignore = paths::load_yuiignore(&source)?;

    let src_path = match find_source_for_target(
        &source,
        &config,
        &target,
        &mut engine,
        &tera_ctx,
        &yuiignore,
    )? {
        Some(s) => s,
        None => anyhow::bail!(
            "no mount entry / .yuilink override claims target {target}; \
                 pass a path inside a known dst"
        ),
    };

    info!("source for {target}: {src_path}");

    if dry_run {
        info!("[dry-run] would absorb {target} → {src_path}");
        return Ok(());
    }

    let backup_root = source.join(&config.backup.dir);
    let ctx = ApplyCtx {
        config: &config,
        source: &source,
        yuiignore: &yuiignore,
        file_mode: resolve_file_mode(config.link.file_mode),
        dir_mode: resolve_dir_mode(config.link.dir_mode),
        backup_root: &backup_root,
        dry_run: false,
    };

    // Manual absorb is an explicit user request — bypass `auto`,
    // `require_clean_git`, and `on_anomaly` policy entirely.
    absorb_target_into_source(&src_path, &target, &ctx)
}

/// Walk mount entries + `.yuilink` Override markers to find the source
/// file/dir that the given target maps back to. Returns `None` when no
/// mount or marker claims the path.
fn find_source_for_target(
    source: &Utf8Path,
    config: &Config,
    target: &Utf8Path,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
    yuiignore: &ignore::gitignore::Gitignore,
) -> Result<Option<Utf8PathBuf>> {
    // 1. Mount entries — render dst, see if target is inside it.
    for entry in &config.mount.entry {
        if let Some(when) = &entry.when {
            if !template::eval_truthy(when, engine, tera_ctx)? {
                continue;
            }
        }
        let dst_str = engine.render(&entry.dst, tera_ctx)?;
        let dst_root = paths::expand_tilde(dst_str.trim());
        if let Ok(rel) = target.strip_prefix(&dst_root) {
            let candidate = source.join(&entry.src).join(rel);
            // Honor `.yuiignore` even on manual absorb — if you've
            // ignored a path, you've explicitly opted out of yui's
            // managing it.
            if paths::is_ignored(yuiignore, source, &candidate, candidate.is_dir()) {
                continue;
            }
            return Ok(Some(candidate));
        }
    }

    // 2. `.yuilink` Override markers — walk source, parse, render each
    //    `[[link]] dst`, see if target is the rendered dst (or nested
    //    inside a junction'd dir). Skips `.yui/` (backup mirrors etc.).
    let walker = paths::source_walker(source).build();
    let marker_filename = &config.mount.marker_filename;
    for ent in walker {
        let ent = match ent {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if ent.path().file_name().and_then(|n| n.to_str()) != Some(marker_filename.as_str()) {
            continue;
        }
        let dir = match ent.path().parent() {
            Some(d) => d,
            None => continue,
        };
        let dir_utf8 = match Utf8PathBuf::from_path_buf(dir.to_path_buf()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if paths::is_ignored(yuiignore, source, &dir_utf8, true) {
            continue;
        }
        let spec = match marker::read_spec(&dir_utf8, marker_filename)? {
            Some(s) => s,
            None => continue,
        };
        let MarkerSpec::Override { links } = spec else {
            continue;
        };
        for link in &links {
            if let Some(when) = &link.when {
                if !template::eval_truthy(when, engine, tera_ctx)? {
                    continue;
                }
            }
            let dst_str = engine.render(&link.dst, tera_ctx)?;
            let dst = paths::expand_tilde(dst_str.trim());
            if target == dst {
                return Ok(Some(dir_utf8));
            }
            if let Ok(rel) = target.strip_prefix(&dst) {
                return Ok(Some(dir_utf8.join(rel)));
            }
        }
    }

    Ok(None)
}

pub fn doctor(source: Option<Utf8PathBuf>) -> Result<()> {
    let yui = YuiVars::detect(Utf8Path::new("."));
    println!("yui doctor");
    println!("==========");
    println!("os:    {}", yui.os);
    println!("arch:  {}", yui.arch);
    println!("user:  {}", yui.user);
    println!("host:  {}", yui.host);
    match resolve_source(source) {
        Ok(s) => {
            println!("source: {s}");
            // Probe: try loading config
            match config::load(&s, &yui) {
                Ok(cfg) => println!(
                    "config: ok ({} mount entries, {} render rules)",
                    cfg.mount.entry.len(),
                    cfg.render.rule.len()
                ),
                Err(e) => println!("config: ERROR — {e}"),
            }
        }
        Err(e) => println!("source: NOT FOUND — {e}"),
    }
    println!();
    println!("link mode (auto resolves to):");
    if cfg!(windows) {
        println!("  files: hardlink");
        println!("  dirs:  junction");
    } else {
        println!("  files: symlink");
        println!("  dirs:  symlink");
    }
    Ok(())
}

pub fn gc_backup(_source: Option<Utf8PathBuf>, _older_than: Option<String>) -> Result<()> {
    todo!("yui gc-backup — clean up old backups")
}

/// `yui hooks list` — show every configured hook + its last-run state.
pub fn hooks_list(source: Option<Utf8PathBuf>) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;
    let state = hook::State::load(&source)?;

    if config.hook.is_empty() {
        println!("(no [[hook]] entries in config)");
        return Ok(());
    }

    for h in &config.hook {
        let phase = match h.phase {
            HookPhase::Pre => "pre",
            HookPhase::Post => "post",
        };
        let when_run = match h.when_run {
            config::WhenRun::Once => "once",
            config::WhenRun::Onchange => "onchange",
            config::WhenRun::Every => "every",
        };
        let last = state
            .hooks
            .get(&h.name)
            .and_then(|s| s.last_run_at.as_deref())
            .unwrap_or("(never)");
        println!(
            "{name:<20}  phase={phase:<4}  when_run={when_run:<8}  last_run_at={last}",
            name = h.name,
        );
        if let Some(when) = &h.when {
            println!("                       when = {when}");
        }
        println!("                       script = {}", h.script);
        println!(
            "                       command = {} {}",
            h.command,
            h.args.join(" ")
        );
    }
    Ok(())
}

/// `yui hooks run [<name>] [--force]` — run a single hook (or every
/// hook) on demand. `--force` bypasses the `when_run` state check;
/// the `when` filter (`yui.os == 'macos'` etc.) is always honored.
pub fn hooks_run(source: Option<Utf8PathBuf>, name: Option<String>, force: bool) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;
    let mut engine = template::Engine::new();
    let tera_ctx = template::template_context(&yui, &config.vars);

    let targets: Vec<&config::HookConfig> = match &name {
        Some(want) => {
            let m = config
                .hook
                .iter()
                .find(|h| &h.name == want)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no [[hook]] named {want:?}; run `yui hooks list` to see available names"
                    )
                })?;
            vec![m]
        }
        None => config.hook.iter().collect(),
    };

    for h in targets {
        let outcome = hook::run_hook(
            h,
            &source,
            &yui,
            &config.vars,
            &mut engine,
            &tera_ctx,
            /* dry_run */ false,
            force,
        )?;
        let label = match outcome {
            HookOutcome::Ran => "ran",
            HookOutcome::SkippedOnce => "skipped (once: already ran)",
            HookOutcome::SkippedUnchanged => "skipped (onchange: hash matches)",
            HookOutcome::SkippedWhenFalse => "skipped (when=false)",
            HookOutcome::DryRun => "would run (dry-run)",
        };
        info!("hook[{}]: {label}", h.name);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

fn process_mount(
    source: &Utf8Path,
    m: &ResolvedMount,
    ctx: &ApplyCtx<'_>,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
) -> Result<()> {
    let src_root = source.join(&m.src);
    if !src_root.is_dir() {
        warn!("mount src missing: {src_root}");
        return Ok(());
    }
    walk_and_link(&src_root, &m.dst, ctx, m.strategy, engine, tera_ctx)
}

fn walk_and_link(
    src_dir: &Utf8Path,
    dst_dir: &Utf8Path,
    ctx: &ApplyCtx<'_>,
    strategy: MountStrategy,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
) -> Result<()> {
    // `.yuiignore` short-circuit — entire subtrees that match are skipped
    // without even reading their marker / iterating their children.
    if paths::is_ignored(ctx.yuiignore, ctx.source, src_dir, /* is_dir */ true) {
        return Ok(());
    }

    let marker_filename = &ctx.config.mount.marker_filename;

    if strategy == MountStrategy::Marker {
        match marker::read_spec(src_dir, marker_filename)? {
            None => {} // no marker — fall through to recursive walk
            Some(MarkerSpec::PassThrough) => {
                link_dir_with_backup(src_dir, dst_dir, ctx)?;
                return Ok(());
            }
            Some(MarkerSpec::Override { links }) => {
                let mut linked_any = false;
                for link in &links {
                    // Nested ifs (not let-chains) so the crate's MSRV
                    // (rust-version = "1.85") stays buildable; let-chains
                    // were stabilized in 1.88.
                    if let Some(when) = &link.when {
                        if !template::eval_truthy(when, engine, tera_ctx)? {
                            continue;
                        }
                    }
                    let dst_str = engine.render(&link.dst, tera_ctx)?;
                    let dst = paths::expand_tilde(dst_str.trim());
                    link_dir_with_backup(src_dir, &dst, ctx)?;
                    linked_any = true;
                }
                if !linked_any {
                    info!("marker override at {src_dir} had no active links — skipping");
                }
                return Ok(());
            }
        }
    }

    for entry in std::fs::read_dir(src_dir)? {
        let entry = entry?;
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        if name == marker_filename {
            continue;
        }
        if name.ends_with(".tera") {
            // Templates are handled by the render flow before linking.
            continue;
        }
        let src_path = src_dir.join(name);
        let dst_path = dst_dir.join(name);
        let ft = entry.file_type()?;

        if paths::is_ignored(ctx.yuiignore, ctx.source, &src_path, ft.is_dir()) {
            continue;
        }

        if ft.is_dir() {
            walk_and_link(&src_path, &dst_path, ctx, strategy, engine, tera_ctx)?;
        } else if ft.is_file() {
            link_file_with_backup(&src_path, &dst_path, ctx)?;
        }
    }
    Ok(())
}

fn link_file_with_backup(src: &Utf8Path, dst: &Utf8Path, ctx: &ApplyCtx<'_>) -> Result<()> {
    use absorb::AbsorbDecision::*;

    let decision = absorb::classify(src, dst)?;

    if ctx.dry_run {
        info!("[dry-run] {decision:?}: {src} → {dst}");
        return Ok(());
    }

    match decision {
        InSync => {
            // Link is intact (same inode/file-id). Nothing to do.
            Ok(())
        }
        Restore => {
            info!("link: {src} → {dst}");
            link::link_file(src, dst, ctx.file_mode)?;
            Ok(())
        }
        RelinkOnly => {
            // Same content, different inode (e.g. hardlink broken by an
            // editor's atomic save). Re-link without touching source.
            info!("relink: {src} → {dst}");
            link::unlink(dst)?;
            link::link_file(src, dst, ctx.file_mode)?;
            Ok(())
        }
        AutoAbsorb => {
            // Target newer + content differs: target wins, source updated.
            // Honor `[absorb] auto` (kill-switch) and `require_clean_git`.
            if !ctx.config.absorb.auto {
                return handle_anomaly(
                    src,
                    dst,
                    ctx,
                    "absorb.auto = false; treating divergence as anomaly",
                );
            }
            if ctx.config.absorb.require_clean_git && !source_repo_is_clean(ctx.source) {
                return handle_anomaly(
                    src,
                    dst,
                    ctx,
                    "source repo is dirty; deferring auto-absorb",
                );
            }
            absorb_target_into_source(src, dst, ctx)
        }
        NeedsConfirm => handle_anomaly(
            src,
            dst,
            ctx,
            "anomaly: source equals/newer than target but content differs",
        ),
    }
}

/// Back up the source-side file, copy the target's content into source,
/// then re-link so the freshly-updated source is what target points at.
/// "Target wins" — yui's core philosophy.
fn absorb_target_into_source(src: &Utf8Path, dst: &Utf8Path, ctx: &ApplyCtx<'_>) -> Result<()> {
    info!("absorb: {dst} → {src}");
    backup_existing(src, ctx.backup_root, /* is_dir */ false)?;
    std::fs::copy(dst, src)?;
    link::unlink(dst)?;
    link::link_file(src, dst, ctx.file_mode)?;
    Ok(())
}

/// Decide what to do for an anomaly (NeedsConfirm or AutoAbsorb that was
/// escalated by `auto = false` / dirty git). Per `[absorb] on_anomaly`:
///   - `skip`  → log warning, leave target alone
///   - `force` → behave like AutoAbsorb (target wins)
///   - `ask`   → on a TTY, show diff + prompt. Off-TTY, downgrade to skip.
fn handle_anomaly(src: &Utf8Path, dst: &Utf8Path, ctx: &ApplyCtx<'_>, reason: &str) -> Result<()> {
    use crate::config::AnomalyAction::*;
    match ctx.config.absorb.on_anomaly {
        Skip => {
            warn!("anomaly skip: {dst} ({reason})");
            Ok(())
        }
        Force => {
            warn!("anomaly force: {dst} ({reason}) — absorbing target into source");
            absorb_target_into_source(src, dst, ctx)
        }
        Ask => {
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                if prompt_absorb_with_diff(src, dst, reason)? {
                    absorb_target_into_source(src, dst, ctx)
                } else {
                    warn!("anomaly skipped by user: {dst}");
                    Ok(())
                }
            } else {
                warn!("anomaly skip (non-TTY ask mode): {dst} ({reason})");
                Ok(())
            }
        }
    }
}

fn prompt_absorb_with_diff(src: &Utf8Path, dst: &Utf8Path, reason: &str) -> Result<bool> {
    use std::io::Write as _;
    let src_content = std::fs::read_to_string(src).unwrap_or_default();
    let dst_content = std::fs::read_to_string(dst).unwrap_or_default();
    eprintln!();
    eprintln!("anomaly: {reason}");
    eprintln!("  src: {src}");
    eprintln!("  dst: {dst}");
    eprintln!();
    eprintln!("--- diff (- source, + target) ---");
    let diff = similar::TextDiff::from_lines(&src_content, &dst_content);
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => "-",
            similar::ChangeTag::Insert => "+",
            similar::ChangeTag::Equal => " ",
        };
        eprint!("{sign}{change}");
    }
    eprintln!();
    eprint!("absorb target into source? [y/N]: ");
    // Flush stderr (where the prompt was written) — flushing stdout was a
    // bug; on a buffered stderr (rare but possible) the prompt would be
    // hidden until after the user typed something. Caught in PR #15
    // review (gemini-code-assist).
    std::io::stderr().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

/// Resilient git-clean check: if `git` isn't available or `source` isn't
/// a repo, log a warning and proceed as if clean. We don't want a missing
/// `git` to block apply — the require_clean_git knob is a *safety net*,
/// not a hard prerequisite.
fn source_repo_is_clean(source: &Utf8Path) -> bool {
    match crate::git::is_clean(source) {
        Ok(b) => b,
        Err(e) => {
            warn!("git clean check failed at {source}: {e} — treating as clean");
            true
        }
    }
}

fn link_dir_with_backup(src: &Utf8Path, dst: &Utf8Path, ctx: &ApplyCtx<'_>) -> Result<()> {
    use absorb::AbsorbDecision::*;
    let decision = absorb::classify(src, dst)?;

    if ctx.dry_run {
        info!("[dry-run] dir {decision:?}: {src} → {dst}");
        return Ok(());
    }

    match decision {
        InSync => Ok(()),
        Restore => {
            info!("link dir: {src} → {dst}");
            link::link_dir(src, dst, ctx.dir_mode)?;
            Ok(())
        }
        _ => {
            // Directory drift: we don't deep-merge the contents (apps can
            // legitimately add files inside a junction'd dir, and yui has
            // no way to know which side is authoritative). Fall back to
            // backup + replace, the same behaviour as before the
            // absorb classifier landed.
            backup_existing(dst, ctx.backup_root, /* is_dir */ true)?;
            link::unlink(dst)?;
            info!("relink dir: {src} → {dst}");
            link::link_dir(src, dst, ctx.dir_mode)?;
            Ok(())
        }
    }
}

fn backup_existing(target: &Utf8Path, backup_root: &Utf8Path, is_dir: bool) -> Result<()> {
    let abs_target = absolutize(target)?;
    let ts = backup::current_timestamp("%Y%m%d_%H%M%S%3f")?;
    let bp = paths::append_timestamp(&paths::mirror_into_backup(backup_root, &abs_target), &ts);
    info!("backup → {bp}");
    if is_dir {
        backup::backup_dir(target, &bp)?;
    } else {
        backup::backup_file(target, &bp)?;
    }
    Ok(())
}

fn resolve_source(source: Option<Utf8PathBuf>) -> Result<Utf8PathBuf> {
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

fn absolutize(p: &Utf8Path) -> Result<Utf8PathBuf> {
    // Expand `~` first so callers can pass `--source ~/dotfiles` directly.
    let expanded = paths::expand_tilde(p.as_str());
    if expanded.is_absolute() {
        return Ok(expanded);
    }
    let cwd = current_dir_utf8()?;
    Ok(cwd.join(expanded))
}

fn current_dir_utf8() -> Result<Utf8PathBuf> {
    let cwd = std::env::current_dir().context("getting cwd")?;
    Utf8PathBuf::from_path_buf(cwd).map_err(|p| anyhow::anyhow!("non-UTF8 cwd: {}", p.display()))
}

// Note: `home_dir()` lives in `paths.rs` so the tilde-expansion helper and
// `resolve_source` share one HOME/USERPROFILE lookup.

const SKELETON_CONFIG: &str = r#"# yui config — see https://github.com/yukimemi/yui

[vars]
# user-defined values; templates can reference these as {{ vars.foo }}

# [link]
# file_mode = "auto"   # auto | symlink | hardlink
# dir_mode  = "auto"   # auto | symlink | junction

[mount]
default_strategy = "marker"

[[mount.entry]]
src = "home"
# `~` expands to $HOME / $USERPROFILE per OS at apply time, no Tera needed.
dst = "~"

# [[mount.entry]]
# src  = "appdata"
# dst  = "{{ env(name='APPDATA') }}"
# # NOTE: write `when` as a *bare* expression (no `{{ … }}`) so it survives
# # config.toml's whole-file Tera render and shows up cleanly in `yui list`.
# when = "yui.os == 'windows'"
"#;

const SKELETON_GITIGNORE: &str = r#"# yui internals (regenerable, do not commit)
/.yui/

# >>> yui rendered (auto-managed, do not edit) >>>
# <<< yui rendered (auto-managed) <<<

# config.local.toml is per-machine; commit a config.local.example.toml instead.
config.local.toml
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn utf8(p: std::path::PathBuf) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(p).unwrap()
    }

    /// Convert a path to a TOML-string-safe form (forward slashes).
    fn toml_path(p: &Utf8Path) -> String {
        p.as_str().replace('\\', "/")
    }

    #[test]
    fn apply_links_a_raw_file() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("home/.bashrc"), "echo hi\n").unwrap();

        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        apply(Some(source), false).unwrap();

        let linked = target.join(".bashrc");
        assert!(linked.exists(), "expected {linked} to exist");
        assert_eq!(std::fs::read_to_string(&linked).unwrap(), "echo hi\n");
    }

    #[test]
    fn apply_with_marker_links_whole_directory() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        let nvim_src = source.join("home/nvim");
        std::fs::create_dir_all(&nvim_src).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(nvim_src.join(".yuilink"), "").unwrap();
        std::fs::write(nvim_src.join("init.lua"), "-- hi\n").unwrap();
        std::fs::write(nvim_src.join("plugins.lua"), "-- plugins\n").unwrap();

        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        apply(Some(source.clone()), false).unwrap();

        let nvim_dst = target.join("nvim");
        assert!(nvim_dst.exists());
        assert_eq!(
            std::fs::read_to_string(nvim_dst.join("init.lua")).unwrap(),
            "-- hi\n"
        );
        // Marker file itself shouldn't be visible as a separate link in target;
        // however with junction/symlink the whole dir shows up so the marker
        // file IS visible inside. That's fine — the marker is informational.
    }

    #[test]
    fn apply_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("home/.bashrc"), "echo hi").unwrap();

        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        apply(Some(source), true).unwrap();

        assert!(!target.join(".bashrc").exists());
    }

    #[test]
    fn apply_renders_templates_then_links_rendered_outputs() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(
            source.join("home/.gitconfig.tera"),
            "[user]\n  os = {{ yui.os }}\n",
        )
        .unwrap();
        std::fs::write(source.join("home/.bashrc"), "raw").unwrap();

        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        apply(Some(source.clone()), false).unwrap();

        // Raw file: linked.
        assert!(target.join(".bashrc").exists());
        // Template's rendered output: written to source then linked.
        assert!(source.join("home/.gitconfig").exists());
        assert!(target.join(".gitconfig").exists());
        // The .tera file itself is never linked into target.
        assert!(!target.join(".gitconfig.tera").exists());
        // Rendered file content carries the yui.os substitution.
        let linked = std::fs::read_to_string(target.join(".gitconfig")).unwrap();
        assert!(linked.contains("os = "));
    }

    #[test]
    fn apply_marker_override_links_to_custom_dst() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target_a = utf8(tmp.path().join("target_a"));
        let target_b = utf8(tmp.path().join("target_b"));
        std::fs::create_dir_all(source.join("home/.config/nvim")).unwrap();
        std::fs::create_dir_all(&target_a).unwrap();
        std::fs::create_dir_all(&target_b).unwrap();
        std::fs::write(
            source.join("home/.config/nvim/init.lua"),
            "-- nvim config\n",
        )
        .unwrap();

        // Marker tells yui to ignore the parent mount's dst for this dir
        // and link it to two custom places (the second only if condition matches).
        std::fs::write(
            source.join("home/.config/nvim/.yuilink"),
            format!(
                r#"
[[link]]
dst = "{}/nvim"

[[link]]
dst = "{}/nvim"
when = "{{{{ yui.os == '{}' }}}}"
"#,
                toml_path(&target_a),
                toml_path(&target_b),
                std::env::consts::OS
            ),
        )
        .unwrap();

        let parent_target = utf8(tmp.path().join("parent_target"));
        std::fs::create_dir_all(&parent_target).unwrap();
        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&parent_target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        apply(Some(source.clone()), false).unwrap();

        // Both override targets received the link (the second's when matches OS).
        assert!(
            target_a.join("nvim/init.lua").exists(),
            "target_a/nvim/init.lua should be reachable through the link"
        );
        assert!(
            target_b.join("nvim/init.lua").exists(),
            "target_b/nvim/init.lua should be reachable through the link"
        );
        // Parent mount did NOT also link this dir (it would have appeared at
        // parent_target/.config/nvim — the marker claims the dir).
        assert!(
            !parent_target.join(".config/nvim").exists(),
            "parent mount should have skipped the marker-claimed sub-dir"
        );
    }

    #[test]
    fn apply_marker_override_skips_inactive_link() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target_inactive = utf8(tmp.path().join("inactive"));
        let parent_target = utf8(tmp.path().join("parent"));
        std::fs::create_dir_all(source.join("home/.config/nvim")).unwrap();
        std::fs::create_dir_all(&parent_target).unwrap();
        std::fs::write(source.join("home/.config/nvim/init.lua"), "x").unwrap();

        // when=false on every link → marker has no active links.
        std::fs::write(
            source.join("home/.config/nvim/.yuilink"),
            format!(
                r#"
[[link]]
dst = "{}/nvim"
when = "{{{{ yui.os == 'no-such-os' }}}}"
"#,
                toml_path(&target_inactive)
            ),
        )
        .unwrap();

        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&parent_target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        apply(Some(source.clone()), false).unwrap();

        // Inactive target untouched.
        assert!(!target_inactive.join("nvim").exists());
        // Parent mount also skipped the dir (marker claims it even when
        // all links are inactive — the user's intent was per-dir override).
        assert!(!parent_target.join(".config/nvim").exists());
    }

    #[test]
    fn list_shows_mount_entries_and_marker_overrides() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(source.join("home/.config/nvim")).unwrap();
        std::fs::write(source.join("home/.config/nvim/init.lua"), "x").unwrap();
        std::fs::write(
            source.join("home/.config/nvim/.yuilink"),
            r#"
[[link]]
dst = "/custom/nvim"
"#,
        )
        .unwrap();
        std::fs::write(
            source.join("config.toml"),
            r#"
[[mount.entry]]
src = "home"
dst = "/h"
"#,
        )
        .unwrap();

        // Just verify it runs without error — output format is covered by
        // unit-level helpers below.
        list(Some(source), false, None, true).unwrap();
    }

    #[test]
    fn status_reports_in_sync_after_apply() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("home/.bashrc"), "echo hi\n").unwrap();
        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();
        // First link the target so the link is intact.
        apply(Some(source.clone()), false).unwrap();
        // status should succeed (everything in-sync).
        status(Some(source), None, true).unwrap();
    }

    #[test]
    fn status_reports_template_drift() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        // Template would render to "fresh" but the rendered file on disk
        // says "stale" — simulating a manual edit not reflected back.
        std::fs::write(source.join("home/.gitconfig.tera"), "fresh").unwrap();
        std::fs::write(source.join("home/.gitconfig"), "stale").unwrap();

        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        let err = status(Some(source), None, true).unwrap_err();
        assert!(format!("{err}").contains("diverged"));
    }

    #[test]
    fn status_fails_when_target_missing() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("home/.bashrc"), "echo hi\n").unwrap();
        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();
        // No apply yet — target/.bashrc doesn't exist.
        let err = status(Some(source), None, true).unwrap_err();
        assert!(format!("{err}").contains("diverged"));
    }

    #[test]
    fn strip_braces_removes_outer_template_braces() {
        assert_eq!(strip_braces("{{ yui.os == 'linux' }}"), "yui.os == 'linux'");
        assert_eq!(strip_braces("yui.os == 'linux'"), "yui.os == 'linux'");
        assert_eq!(strip_braces("  {{x}}  "), "x");
    }

    #[test]
    fn apply_aborts_on_render_drift() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("home/foo.tera"), "fresh body").unwrap();
        std::fs::write(source.join("home/foo"), "manually edited").unwrap();

        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        let err = apply(Some(source.clone()), false).unwrap_err();
        assert!(format!("{err}").contains("drift"));
        // Existing rendered file untouched.
        assert_eq!(
            std::fs::read_to_string(source.join("home/foo")).unwrap(),
            "manually edited"
        );
        // Linking aborted — target empty.
        assert!(!target.join("foo").exists());
    }

    #[test]
    fn init_creates_skeleton_when_dir_empty() {
        let tmp = TempDir::new().unwrap();
        let dir = utf8(tmp.path().join("new_dotfiles"));
        init(Some(dir.clone()), false).unwrap();
        assert!(dir.join("config.toml").is_file());
        assert!(dir.join(".gitignore").is_file());
    }

    #[test]
    fn init_refuses_to_overwrite_existing_config() {
        let tmp = TempDir::new().unwrap();
        let dir = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "preexisting").unwrap();
        let err = init(Some(dir), false).unwrap_err();
        assert!(format!("{err}").contains("already exists"));
    }

    /// Build a minimal `apply`-able dotfiles tree for absorb tests.
    /// Returns (source_dir, target_dir).
    fn setup_minimal_dotfiles(tmp: &TempDir) -> (Utf8PathBuf, Utf8PathBuf) {
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();
        (source, target)
    }

    fn write_with_mtime(path: &Utf8Path, body: &str, when: std::time::SystemTime) {
        std::fs::write(path, body).unwrap();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open writable");
        f.set_modified(when).expect("set_modified");
    }

    #[test]
    fn apply_target_newer_absorbs_target_into_source() {
        // Target has the user's edit and is mtime-newer than source —
        // classifier returns `AutoAbsorb`. yui's "target-as-truth"
        // philosophy: target wins, source is updated and backed up.
        let tmp = TempDir::new().unwrap();
        let (source, target) = setup_minimal_dotfiles(&tmp);

        let now = std::time::SystemTime::now();
        let past = now - std::time::Duration::from_secs(120);
        write_with_mtime(&source.join("home/.bashrc"), "default from repo", past);
        // Pre-existing target with user's edit, NEWER mtime.
        write_with_mtime(&target.join(".bashrc"), "user's edit", now);

        apply(Some(source.clone()), false).unwrap();

        // Target's content survives — that's the whole point.
        assert_eq!(
            std::fs::read_to_string(target.join(".bashrc")).unwrap(),
            "user's edit"
        );
        // Source has been updated to match target.
        assert_eq!(
            std::fs::read_to_string(source.join("home/.bashrc")).unwrap(),
            "user's edit"
        );
        // Source's previous content lives under .yui/backup.
        let backup_root = source.join(".yui/backup");
        let mut found_old = false;
        for entry in walkdir(&backup_root) {
            if let Ok(s) = std::fs::read_to_string(&entry) {
                if s == "default from repo" {
                    found_old = true;
                    break;
                }
            }
        }
        assert!(found_old, "expected backup containing 'default from repo'");
    }

    #[test]
    fn apply_in_sync_target_is_a_no_op() {
        // After an initial `apply`, running `apply` again classifies as
        // `InSync` and does nothing.
        let tmp = TempDir::new().unwrap();
        let (source, target) = setup_minimal_dotfiles(&tmp);
        std::fs::write(source.join("home/.bashrc"), "echo hi\n").unwrap();
        apply(Some(source.clone()), false).unwrap();
        let backup_root = source.join(".yui/backup");
        let backup_count_after_first = walkdir(&backup_root).len();

        // Second apply — nothing should change.
        apply(Some(source.clone()), false).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join(".bashrc")).unwrap(),
            "echo hi\n"
        );
        let backup_count_after_second = walkdir(&backup_root).len();
        assert_eq!(
            backup_count_after_first, backup_count_after_second,
            "second apply on an in-sync tree should not produce backups"
        );
    }

    #[test]
    fn apply_skip_policy_leaves_anomaly_alone() {
        // Source newer than target + content differs = NeedsConfirm.
        // With on_anomaly = "skip", target stays untouched.
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let cfg = format!(
            r#"
[absorb]
on_anomaly = "skip"

[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        let now = std::time::SystemTime::now();
        let past = now - std::time::Duration::from_secs(120);
        write_with_mtime(&target.join(".bashrc"), "user's edit (older)", past);
        write_with_mtime(&source.join("home/.bashrc"), "fresh from upstream", now);

        apply(Some(source.clone()), false).unwrap();

        // Target untouched (skip policy honored).
        assert_eq!(
            std::fs::read_to_string(target.join(".bashrc")).unwrap(),
            "user's edit (older)"
        );
        // Source untouched too.
        assert_eq!(
            std::fs::read_to_string(source.join("home/.bashrc")).unwrap(),
            "fresh from upstream"
        );
    }

    #[test]
    fn apply_force_policy_absorbs_anomaly_anyway() {
        // Same anomaly setup, but on_anomaly = "force" → target wins.
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let cfg = format!(
            r#"
[absorb]
on_anomaly = "force"

[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        let now = std::time::SystemTime::now();
        let past = now - std::time::Duration::from_secs(120);
        write_with_mtime(&target.join(".bashrc"), "user's edit (older)", past);
        write_with_mtime(&source.join("home/.bashrc"), "fresh from upstream", now);

        apply(Some(source.clone()), false).unwrap();

        // Target wins despite being mtime-older — force policy.
        assert_eq!(
            std::fs::read_to_string(target.join(".bashrc")).unwrap(),
            "user's edit (older)"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("home/.bashrc")).unwrap(),
            "user's edit (older)"
        );
    }

    #[test]
    fn manual_absorb_command_pulls_target_into_source() {
        // Manual `yui absorb <target>` bypasses policy + git checks.
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        // on_anomaly = "skip" so passive `apply` would NOT touch this.
        let cfg = format!(
            r#"
[absorb]
on_anomaly = "skip"

[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();
        std::fs::write(target.join(".bashrc"), "user picked this").unwrap();
        std::fs::write(source.join("home/.bashrc"), "default").unwrap();

        // Run absorb directly on the target.
        absorb(
            Some(source.clone()),
            target.join(".bashrc"),
            /* dry_run */ false,
        )
        .unwrap();

        // Source picked up target's content (manual absorb is forceful).
        assert_eq!(
            std::fs::read_to_string(source.join("home/.bashrc")).unwrap(),
            "user picked this"
        );
    }

    #[test]
    fn manual_absorb_errors_when_target_outside_known_mounts() {
        let tmp = TempDir::new().unwrap();
        let (source, _target) = setup_minimal_dotfiles(&tmp);
        std::fs::write(source.join("home/.bashrc"), "x").unwrap();
        let stranger = utf8(tmp.path().join("not-managed/foo"));
        std::fs::create_dir_all(stranger.parent().unwrap()).unwrap();
        std::fs::write(&stranger, "not yui's").unwrap();
        let err = absorb(Some(source), stranger, false).unwrap_err();
        assert!(format!("{err}").contains("no mount entry"));
    }

    #[test]
    fn yuiignore_excludes_file_from_linking() {
        let tmp = TempDir::new().unwrap();
        let (source, target) = setup_minimal_dotfiles(&tmp);
        std::fs::write(source.join("home/.bashrc"), "kept").unwrap();
        std::fs::write(source.join("home/lock.json"), "ignored").unwrap();
        // Exclude `lock.json` files anywhere under source.
        std::fs::write(source.join(".yuiignore"), "**/lock.json\n").unwrap();
        apply(Some(source.clone()), false).unwrap();
        assert!(target.join(".bashrc").exists());
        assert!(
            !target.join("lock.json").exists(),
            "yuiignore should keep lock.json out of target"
        );
    }

    #[test]
    fn yuiignore_excludes_directory_subtree() {
        let tmp = TempDir::new().unwrap();
        let (source, target) = setup_minimal_dotfiles(&tmp);
        std::fs::create_dir_all(source.join("home/cache")).unwrap();
        std::fs::write(source.join("home/.bashrc"), "kept").unwrap();
        std::fs::write(source.join("home/cache/a"), "ignored").unwrap();
        std::fs::write(source.join("home/cache/b"), "also ignored").unwrap();
        // Trailing slash → match dirs only; entire subtree skipped.
        std::fs::write(source.join(".yuiignore"), "home/cache/\n").unwrap();
        apply(Some(source.clone()), false).unwrap();
        assert!(target.join(".bashrc").exists());
        assert!(
            !target.join("cache").exists(),
            "yuiignore'd subtree should not appear in target"
        );
    }

    #[test]
    fn yuiignore_negation_re_includes_file() {
        let tmp = TempDir::new().unwrap();
        let (source, target) = setup_minimal_dotfiles(&tmp);
        std::fs::write(source.join("home/keep.cache"), "kept by negation").unwrap();
        std::fs::write(source.join("home/drop.cache"), "ignored").unwrap();
        // Ignore all .cache files except keep.cache.
        std::fs::write(source.join(".yuiignore"), "*.cache\n!keep.cache\n").unwrap();
        apply(Some(source.clone()), false).unwrap();
        assert!(target.join("keep.cache").exists());
        assert!(!target.join("drop.cache").exists());
    }

    #[test]
    fn yuiignore_skips_template_in_render() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("home/note.tera"), "{{ yui.os }}").unwrap();
        std::fs::write(source.join(".yuiignore"), "home/note*\n").unwrap();
        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();
        apply(Some(source.clone()), false).unwrap();
        // Neither the template nor the rendered output linked.
        assert!(!source.join("home/note").exists());
        assert!(!target.join("note").exists());
        assert!(!target.join("note.tera").exists());
    }

    fn walkdir(root: &Utf8Path) -> Vec<Utf8PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = utf8(e.path());
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(p);
                } else {
                    out.push(p);
                }
            }
        }
        out
    }
}
