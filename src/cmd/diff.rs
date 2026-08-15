use super::*;
use crate::config::{self, Config, IconsMode};
use crate::icons::Icons;
use crate::links::LinkPlan;
use crate::mount;
use crate::render;
use crate::template;
use crate::vars::YuiVars;
use crate::{absorb, paths};
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

/// `yui diff [--icons MODE] [--no-color]` — for every drifted entry
/// (link, render, or secret), print a unified diff to stdout.
///
/// Layered on top of the same drift detection `yui status` uses
/// (`absorb::classify` + render dry-run), but actually emits the
/// content delta. InSync / Restore / RelinkOnly entries are
/// suppressed — they're not "drift the user can read".
pub fn diff(
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

    let _icons = Icons::for_mode(icons_override.unwrap_or(config.ui.icons));
    let color = !no_color && supports_color_stdout();

    // Reuse classify_walk to enumerate every src→dst pair.
    let mut report: Vec<StatusItem> = Vec::new();
    let mut yuiignore = paths::YuiIgnoreStack::with_gitignore(config.mount.respect_gitignore);
    yuiignore.push_dir(&source)?;
    let plan = LinkPlan::from_config(&source, &config.link)?;
    let walk_result = (|| -> Result<()> {
        for m in &mounts {
            let src_root = m.src.clone();
            if !src_root.is_dir() {
                continue;
            }
            classify_walk(
                &src_root,
                &m.dst,
                &config,
                &plan,
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

    // Render-drift surfaces too — same as cmd::status.
    let render_report = render::render_all(&source, &config, &yui, /* dry_run */ true)?;
    for entry in &render_report.diverged {
        report.push(StatusItem {
            src: entry.tera_path.clone(),
            dst: entry.rendered_path.clone(),
            state: StatusState::RenderDrift,
        });
    }

    // Secret drift too — same downgrade-to-warning semantics as
    // cmd::status so a broken identity doesn't kill the diff.
    match crate::secret::decrypt_all(&source, &config, /* dry_run */ true) {
        Ok(secret_report) => {
            for plaintext in &secret_report.diverged {
                report.push(StatusItem {
                    src: crate::secret::age_sibling(plaintext),
                    dst: plaintext.clone(),
                    state: StatusState::SecretDrift,
                });
            }
        }
        Err(e) => tracing::warn!("secret drift check skipped: {e}"),
    }

    let mut printed = 0usize;
    for item in &report {
        if !diff_worth_printing(&item.state) {
            continue;
        }
        let src_abs = resolve_diff_src(item, &source);
        print_unified_diff(
            &src_abs,
            &item.dst,
            &item.state,
            &source,
            &config,
            &yui,
            color,
        );
        printed += 1;
    }

    if printed == 0 {
        println!("  no diff — every entry is in sync (or only needs a relink)");
    } else {
        println!();
        println!(
            "  {printed} entr{} with content drift",
            if printed == 1 { "y" } else { "ies" }
        );
    }
    Ok(())
}

/// Resolve a `StatusItem.src` to an absolute path suitable for
/// reading from disk during diff rendering.
///
/// `classify_walk` stores `StatusItem.src` via
/// `relative_for_display(...)`, which strips the source-root prefix
/// for table rendering. For `Link(_)` rows we have to re-absolutize
/// before reading — otherwise the path resolves against the
/// caller's cwd and we'd read an empty / wrong file. `RenderDrift`
/// and `SecretDrift` rows already carry an absolute path (built
/// from the dry-run reports, which the walkers yield as absolute).
/// (Caught in PR #53 review by coderabbitai.)
pub(crate) fn resolve_diff_src(item: &StatusItem, source: &Utf8Path) -> Utf8PathBuf {
    match item.state {
        StatusState::RenderDrift | StatusState::SecretDrift => item.src.clone(),
        StatusState::Link(_) => source.join(&item.src),
    }
}

pub(crate) fn diff_worth_printing(state: &StatusState) -> bool {
    use absorb::AbsorbDecision::*;
    match state {
        StatusState::Link(InSync) => false,
        StatusState::Link(Restore) => false, // target missing — nothing to diff
        StatusState::Link(RelinkOnly) => false, // content identical, only metadata drift
        StatusState::Link(_) => true,
        StatusState::RenderDrift => true,
        StatusState::SecretDrift => true,
    }
}

/// `src` is the .tera path for `RenderDrift` rows and the source
/// file/dir for `Link(_)` rows. For RenderDrift we render the
/// template to a string and diff that against the on-disk
/// rendered file — diffing the raw .tera against the rendered
/// output would surface Tera's `{{ }}` syntax as drift instead
/// of the actual content delta. (Caught in PR #53 review by
/// gemini-code-assist.)
fn print_unified_diff(
    src: &Utf8Path,
    dst: &Utf8Path,
    state: &StatusState,
    source_root: &Utf8Path,
    config: &Config,
    yui: &YuiVars,
    color: bool,
) {
    use owo_colors::OwoColorize as _;

    let header = match state {
        StatusState::RenderDrift => format!("--- render drift: {src} (template) vs {dst}"),
        StatusState::SecretDrift => format!("--- secret drift: {src} (decrypted) vs {dst}"),
        _ => format!("--- {src} → {dst}"),
    };
    if color {
        println!("{}", header.bold());
    } else {
        println!("{header}");
    }

    if src.is_dir() || dst.is_dir() {
        println!("(directory entry — content listing skipped)");
        println!();
        return;
    }

    // Source side of the diff:
    //   - RenderDrift → re-render the .tera in memory (otherwise
    //     we'd surface raw Tera syntax as drift).
    //   - SecretDrift → decrypt the .age in memory (diffing raw
    //     ciphertext would be meaningless).
    //   - Link(_)     → read the source file from disk.
    let src_content = match state {
        StatusState::SecretDrift => match crate::secret::decrypt_file(src, config) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => {
                    println!("(binary secret — diff skipped)");
                    println!();
                    return;
                }
            },
            Err(e) => {
                println!("(error decrypting {src}: {e})");
                println!();
                return;
            }
        },
        StatusState::RenderDrift => match render::render_to_string(src, source_root, config, yui) {
            Ok(Some(s)) => s,
            Ok(None) => {
                println!(
                    "(template would be skipped on this host — drift will resolve on next render)"
                );
                println!();
                return;
            }
            Err(e) => {
                println!("(error rendering template: {e})");
                println!();
                return;
            }
        },
        _ => match read_text_for_diff(src) {
            DiffSide::Text(s) => s,
            DiffSide::Binary => {
                println!("(binary file or non-UTF-8 content — diff skipped)");
                println!();
                return;
            }
        },
    };
    let dst_content = match read_text_for_diff(dst) {
        DiffSide::Text(s) => s,
        DiffSide::Binary => {
            println!("(binary file or non-UTF-8 content — diff skipped)");
            println!();
            return;
        }
    };
    print_unified_text_diff(
        &src_content,
        &dst_content,
        src.as_str(),
        dst.as_str(),
        color,
    );
    println!();
}

/// Render a true unified diff (with `@@` hunk headers + 3-line
/// context windows) via `similar::TextDiff::unified_diff` and
/// route each line to stdout — colour the `+` / `-` / `@@` lines
/// when the caller asked for it. Both `yui diff` and the absorb
/// flow share this so the format is consistent regardless of
/// entry point. (PR #53 review tightened the contract from the
/// hand-rolled prefix loop to the standard `unified_diff`
/// formatter.)
fn print_unified_text_diff(src: &str, dst: &str, src_label: &str, dst_label: &str, color: bool) {
    use owo_colors::OwoColorize as _;
    let diff = similar::TextDiff::from_lines(src, dst);
    let formatted = diff.unified_diff().header(src_label, dst_label).to_string();
    for line in formatted.lines() {
        if !color {
            println!("{line}");
        } else if line.starts_with("+++") || line.starts_with("---") {
            println!("{}", line.dimmed());
        } else if line.starts_with("@@") {
            println!("{}", line.cyan());
        } else if line.starts_with('+') {
            println!("{}", line.green());
        } else if line.starts_with('-') {
            println!("{}", line.red());
        } else {
            println!("{line}");
        }
    }
}
