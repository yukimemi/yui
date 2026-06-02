use super::*;
use crate::config::{self, HookPhase, IconsMode};
use crate::hook::{self, HookOutcome};
use crate::icons::Icons;
use crate::template;
use crate::vars::YuiVars;
use anyhow::Result;
use camino::Utf8PathBuf;
use tracing::info;

/// `yui hooks list` — show every configured hook + its last-run state.
pub fn hooks_list(
    source: Option<Utf8PathBuf>,
    icons_override: Option<IconsMode>,
    no_color: bool,
) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;
    let state = hook::State::load(&source)?;

    let icons_mode = icons_override.unwrap_or(config.ui.icons);
    let icons = Icons::for_mode(icons_mode);
    let color = !no_color && supports_color_stdout();

    if config.hook.is_empty() {
        println!("(no [[hook]] entries in config)");
        return Ok(());
    }

    // Pre-evaluate the `when` filter for every hook so the status icon
    // can distinguish "skipped because the OS gate is false" from
    // "active but never run".
    let mut engine = template::Engine::new();
    let tera_ctx = template::template_context(&yui, &config.vars);
    let rows: Vec<HookRow> = config
        .hook
        .iter()
        .map(|h| -> Result<HookRow> {
            // Propagate Tera errors instead of silently coercing them
            // to "inactive" — a syntax error in the user's `when`
            // expression should surface, not hide.
            let active = match &h.when {
                None => true,
                Some(w) => template::eval_truthy(w, &mut engine, &tera_ctx)?,
            };
            let last_run_at = state.hooks.get(&h.name).and_then(|s| s.last_run_at.clone());
            Ok(HookRow {
                name: h.name.clone(),
                phase: match h.phase {
                    HookPhase::Pre => "pre",
                    HookPhase::Post => "post",
                },
                when_run: match h.when_run {
                    config::WhenRun::Once => "once",
                    config::WhenRun::Onchange => "onchange",
                    config::WhenRun::Every => "every",
                },
                last_run_at,
                when: h.when.clone(),
                active,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    print_hooks_table(&rows, icons, color);

    let total = rows.len();
    let active = rows.iter().filter(|r| r.active).count();
    let inactive = total - active;
    let ran = rows.iter().filter(|r| r.last_run_at.is_some()).count();
    let never = total - ran;
    println!();
    println!(
        "  {total} hooks · {active} active · {inactive} inactive · {ran} ran · {never} never run"
    );

    Ok(())
}

#[derive(Debug)]
struct HookRow {
    name: String,
    phase: &'static str,
    when_run: &'static str,
    last_run_at: Option<String>,
    when: Option<String>,
    active: bool,
}

fn print_hooks_table(rows: &[HookRow], icons: Icons, color: bool) {
    use owo_colors::OwoColorize as _;
    use std::fmt::Write as _;

    let name_w = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let phase_w = rows
        .iter()
        .map(|r| r.phase.len())
        .max()
        .unwrap_or(0)
        .max("PHASE".len());
    let when_run_w = rows
        .iter()
        .map(|r| r.when_run.len())
        .max()
        .unwrap_or(0)
        .max("WHEN_RUN".len());
    let last_w = rows
        .iter()
        .map(|r| {
            r.last_run_at
                .as_deref()
                .map(|s| s.chars().count())
                .unwrap_or("(never)".len())
        })
        .max()
        .unwrap_or(0)
        .max("LAST_RUN".len());
    let status_w = "STATUS".len();

    // Header
    let mut header = String::new();
    let _ = write!(
        &mut header,
        "  {:<status_w$}  {:<name_w$}  {:<phase_w$}  {:<when_run_w$}  {:<last_w$}  WHEN",
        "STATUS", "NAME", "PHASE", "WHEN_RUN", "LAST_RUN"
    );
    if color {
        println!("{}", header.bold());
    } else {
        println!("{header}");
    }

    // Separator (re-uses the same sep glyph the list / status table picks).
    let bar = |n: usize| icons.sep.to_string().repeat(n);
    let sep = format!(
        "  {}  {}  {}  {}  {}  {}",
        bar(status_w),
        bar(name_w),
        bar(phase_w),
        bar(when_run_w),
        bar(last_w),
        bar("WHEN".len())
    );
    if color {
        println!("{}", sep.dimmed());
    } else {
        println!("{sep}");
    }

    // Rows
    for r in rows {
        // Status icon picks one of three states. We could expand this
        // (✗ failed, ↻ would-rerun-via-onchange-hash) once `hooks list`
        // grows enough fields to justify it; today's set is enough to
        // make the table scannable.
        let (icon, ran) = match (r.active, r.last_run_at.is_some()) {
            (false, _) => (icons.inactive, false),
            (true, true) => (icons.active, true),
            (true, false) => (icons.info, false),
        };
        let last = r.last_run_at.as_deref().unwrap_or("(never)");
        let when_str = r
            .when
            .as_deref()
            .map(strip_braces)
            .unwrap_or_else(|| "(always)".to_string());

        let cell_status = format!("{icon:<status_w$}");
        let cell_name = format!("{:<name_w$}", r.name);
        let cell_phase = format!("{:<phase_w$}", r.phase);
        let cell_when_run = format!("{:<when_run_w$}", r.when_run);
        let cell_last = format!("{last:<last_w$}");

        if !color {
            println!(
                "  {cell_status}  {cell_name}  {cell_phase}  {cell_when_run}  {cell_last}  {when_str}"
            );
            continue;
        }

        // Active+ran: green status, bold name. Active-but-never: yellow
        // status (the "🆕 new — apply hasn't ticked it" signal). Inactive
        // (when-false): dimmed across the row.
        if !r.active {
            println!(
                "  {}  {}  {}  {}  {}  {}",
                cell_status.dimmed(),
                cell_name.dimmed(),
                cell_phase.dimmed(),
                cell_when_run.dimmed(),
                cell_last.dimmed(),
                when_str.dimmed()
            );
        } else if ran {
            println!(
                "  {}  {}  {}  {}  {}  {}",
                cell_status.green(),
                cell_name.cyan().bold(),
                cell_phase.dimmed(),
                cell_when_run.dimmed(),
                cell_last.green(),
                when_str.dimmed()
            );
        } else {
            println!(
                "  {}  {}  {}  {}  {}  {}",
                cell_status.yellow(),
                cell_name.cyan().bold(),
                cell_phase.dimmed(),
                cell_when_run.dimmed(),
                cell_last.yellow(),
                when_str.dimmed()
            );
        }
    }
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

    let mut state = hook::State::load(&source)?;
    for h in targets {
        let outcome = hook::run_hook(
            h,
            &source,
            &yui,
            &config.vars,
            &mut engine,
            &tera_ctx,
            &mut state,
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
        if outcome == HookOutcome::Ran {
            state.save(&source)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------
