use super::*;
use crate::config::{self, Config, HookPhase, MountStrategy};
use crate::hook;
use crate::link::{self, EffectiveDirMode, EffectiveFileMode, resolve_dir_mode, resolve_file_mode};
use crate::marker::{self, MarkerSpec};
use crate::mount::{self, ResolvedMount};
use crate::render::{self, RenderReport};
use crate::secret;
use crate::template;
use crate::vars::YuiVars;
use crate::{absorb, backup, paths};
use anyhow::{Context as _, Result};
use camino::{Utf8Path, Utf8PathBuf};
use std::cell::Cell;
use teravars::Context as TeraContext;
use tracing::{info, warn};

pub fn apply(source: Option<Utf8PathBuf>, dry_run: bool) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

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

    // 1a. Decrypt `*.age` files first — the rendered templates
    //     might `{{ ... }}`-reference plaintext siblings indirectly
    //     (via env vars set by hooks), and even when they don't,
    //     decrypting first keeps the order of "physical sibling
    //     files appear" predictable.
    let secret_report = secret::decrypt_all(&source, &config, dry_run)?;
    log_secret_report(&secret_report);
    if secret_report.has_drift() {
        anyhow::bail!(
            "secret drift detected ({} file(s)); the plaintext sibling diverged \
             from the canonical .age — run `yui secret encrypt <path>` to roll \
             the edit back into ciphertext before re-running apply",
            secret_report.diverged.len()
        );
    }

    // 1b. Render templates so the link walk picks up rendered files.
    //     Drift is resolved interactively (`[o]verwrite` / `[s]kip`) so the
    //     "I just edited the `.tera`" / "I just changed `vars`" / "I want
    //     `vars` substitution to land in the rendered file" cases don't
    //     dead-end at a `bail!`. Dry-run only logs; the prompt never fires
    //     so apply previews stay non-interactive.
    let render_report = render::render_all(&source, &config, &yui, dry_run)?;
    log_render_report(&render_report);
    let render_quit: Cell<bool> = Cell::new(false);
    if render_report.has_drift() && !dry_run {
        resolve_render_drift(&render_report, &render_quit)?;
    }
    if render_quit.get() {
        info!("user quit during render drift resolution; skipping link pass");
        return Ok(());
    }

    // 1c. Single deterministic write of the `.gitignore` managed
    //     section, covering both `*.tera` outputs and `*.age`
    //     plaintext siblings. (Earlier this was two writes — once
    //     inside `render_all`, once here — which made the managed
    //     section flicker if a reader read between them. PR #57
    //     review caught it; render_all no longer touches gitignore.)
    if !dry_run && config.render.manage_gitignore {
        let mut managed: Vec<Utf8PathBuf> = render::report_managed_paths(&render_report)
            .into_iter()
            .chain(secret_report.managed_paths().cloned())
            .collect();
        managed.sort();
        managed.dedup();
        render::write_managed_section(&source, &managed)?;
    }

    // 2. Resolve mounts and link.
    let mounts = mount::resolve(
        &source,
        &config.mount.entry,
        config.mount.default_strategy,
        &mut engine,
        &tera_ctx,
    )?;

    let backup_root = source.join(&config.backup.dir);
    let ctx = ApplyCtx {
        config: &config,
        source: &source,
        file_mode: resolve_file_mode(config.link.file_mode),
        dir_mode: resolve_dir_mode(config.link.dir_mode),
        backup_root: &backup_root,
        dry_run,
        sticky_anomaly: Cell::new(None),
        quit_requested: Cell::new(false),
    };

    info!("source: {source}");
    info!("modes: file={:?} dir={:?}", ctx.file_mode, ctx.dir_mode);
    if dry_run {
        info!("dry-run: nothing will be written");
    }

    // Nested ignore stack — push on dir entry, pop on exit. Seed
    // with the source-root layer so root-level rules apply from the
    // start without `walk_and_link` having to special-case it.
    let mut yuiignore = paths::YuiIgnoreStack::with_gitignore(config.mount.respect_gitignore);
    yuiignore.push_dir(&source)?;
    let walk_result = (|| -> Result<()> {
        for m in &mounts {
            info!("mount: {} → {}", m.src, m.dst);
            process_mount(m, &ctx, &mut engine, &tera_ctx, &mut yuiignore)?;
        }
        Ok(())
    })();
    yuiignore.pop_dir(&source);
    walk_result?;

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

pub(crate) fn log_render_report(r: &RenderReport) {
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
        warn!("rendered file diverged from template: {}", d.rendered_path);
    }
}

fn log_secret_report(r: &secret::SecretReport) {
    if !r.written.is_empty() {
        info!("decrypted {} secret file(s)", r.written.len());
    }
    if !r.unchanged.is_empty() {
        info!("decrypted {} secret(s) unchanged", r.unchanged.len());
    }
    for d in &r.diverged {
        warn!("plaintext sibling diverged from .age: {d}");
    }
}

/// Bundle of immutable settings threaded through the apply walk.
///
/// `.yuiignore` rules are not in here — they need a `&mut` stack
/// (push on dir entry, pop on dir exit) which doesn't compose with
/// `ApplyCtx` being shared by `&`. The stack is plumbed through
/// `walk_and_link` as its own parameter instead.
/// User-chosen direction for an `[absorb] on_anomaly = "ask"` prompt.
///
/// "Absorb" matches yui's default flow (target wins, content lands in
/// source). "Overwrite" is the inverse for cases where the user just
/// edited source intentionally and wants target updated to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnomalyChoice {
    /// target → source (yui's default, "target is truth").
    Absorb,
    /// source → target (user-edited source wins, target updated).
    Overwrite,
    /// Leave both as-is for now.
    Skip,
    /// Skip this entry and stop walking remaining entries.
    Quit,
}

/// User-chosen direction for a render-drift prompt.
///
/// Render drift has no `[a] absorb` direction: rendered files have
/// already had Tera substitutions applied, so writing them back over
/// the `.tera` source would silently erase the template syntax. A
/// user who wants the on-disk rendered content reflected into the
/// template picks `[s]kip` and edits the `.tera` by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderDriftChoice {
    /// Write the fresh template output over the on-disk rendered file.
    Overwrite,
    /// Leave both as-is. The link pass may still relink afterwards.
    Skip,
    /// Skip this entry and stop walking remaining render-drift entries.
    Quit,
}

pub(crate) struct ApplyCtx<'a> {
    pub(crate) config: &'a Config,
    /// Source repo root — needed for git-clean checks during absorb.
    pub(crate) source: &'a Utf8Path,
    pub(crate) file_mode: EffectiveFileMode,
    pub(crate) dir_mode: EffectiveDirMode,
    pub(crate) backup_root: &'a Utf8Path,
    pub(crate) dry_run: bool,
    /// Sticky decision from a previous "all" prompt. When set, every
    /// subsequent anomaly applies this choice without prompting.
    pub(crate) sticky_anomaly: Cell<Option<AnomalyChoice>>,
    /// Set by the `[q]uit` choice. The walker checks this at the top
    /// of every link op and short-circuits to a no-op so apply exits
    /// cleanly without further prompts.
    pub(crate) quit_requested: Cell<bool>,
}

#[allow(clippy::too_many_arguments)]
fn process_mount(
    m: &ResolvedMount,
    ctx: &ApplyCtx<'_>,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
    yuiignore: &mut paths::YuiIgnoreStack,
) -> Result<()> {
    // `m.src` is already absolute (resolved by `mount::resolve`),
    // so we don't need the source-root anymore.
    let src_root = m.src.clone();
    if !src_root.is_dir() {
        warn!("mount src missing: {src_root}");
        return Ok(());
    }
    walk_and_link(
        &src_root, &m.dst, ctx, m.strategy, engine, tera_ctx, yuiignore, false,
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_and_link(
    src_dir: &Utf8Path,
    dst_dir: &Utf8Path,
    ctx: &ApplyCtx<'_>,
    strategy: MountStrategy,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
    yuiignore: &mut paths::YuiIgnoreStack,
    parent_covered: bool,
) -> Result<()> {
    // `.yuiignore` short-circuit — entire subtrees that match are skipped
    // without even reading their marker / iterating their children.
    if yuiignore.is_ignored(src_dir, /* is_dir */ true) {
        return Ok(());
    }
    // Layer this dir's `.yuiignore` (if any) on top, run the body, pop
    // before returning so siblings don't see our subtree's rules.
    yuiignore.push_dir(src_dir)?;
    let result = walk_and_link_body(
        src_dir,
        dst_dir,
        ctx,
        strategy,
        engine,
        tera_ctx,
        yuiignore,
        parent_covered,
    );
    yuiignore.pop_dir(src_dir);
    result
}

#[allow(clippy::too_many_arguments)]
fn walk_and_link_body(
    src_dir: &Utf8Path,
    dst_dir: &Utf8Path,
    ctx: &ApplyCtx<'_>,
    strategy: MountStrategy,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
    yuiignore: &mut paths::YuiIgnoreStack,
    parent_covered: bool,
) -> Result<()> {
    let marker_filename = &ctx.config.mount.marker_filename;
    let mut covered = parent_covered;

    if strategy == MountStrategy::Marker {
        match marker::read_spec(src_dir, marker_filename)? {
            None => {} // no marker — fall through to recursive walk
            Some(MarkerSpec::PassThrough) => {
                // Empty marker = junction this dir at the natural
                // mount-derived dst. Subsequent recursion keeps going so
                // descendant markers can layer on extra dsts.
                link_dir_with_backup(src_dir, dst_dir, ctx)?;
                covered = true;
            }
            Some(MarkerSpec::Explicit { links }) => {
                let mut emitted_dir_link = false;
                let mut emitted_any = false;
                for link in &links {
                    // Nested ifs (not let-chains) so the crate's MSRV
                    // (rust-version = "1.85") stays buildable.
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
                        link_file_with_backup(&file_src, &dst, ctx)?;
                    } else {
                        link_dir_with_backup(src_dir, &dst, ctx)?;
                        emitted_dir_link = true;
                    }
                    emitted_any = true;
                }
                if !emitted_any {
                    // v0.6+ semantics: with no active links, the walker
                    // still descends and per-file defaults still apply.
                    // Phrase it so users don't read "skipping" as
                    // "subtree blocked" (the v0.5 behaviour).
                    info!(
                        "marker at {src_dir} had no active links \
                         — falling back to defaults"
                    );
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

        if yuiignore.is_ignored(&src_path, ft.is_dir()) {
            continue;
        }

        if ft.is_dir() {
            walk_and_link(
                &src_path, &dst_path, ctx, strategy, engine, tera_ctx, yuiignore, covered,
            )?;
        } else if ft.is_file() {
            // If an ancestor (or this dir itself) created a dir-level
            // junction, the file is already accessible via that junction
            // — emitting another per-file link would just duplicate work
            // (and on Windows might land at a path that's already
            // hard-linked through the parent).
            if !covered {
                link_file_with_backup(&src_path, &dst_path, ctx)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn link_file_with_backup(
    src: &Utf8Path,
    dst: &Utf8Path,
    ctx: &ApplyCtx<'_>,
) -> Result<()> {
    use absorb::AbsorbDecision::*;

    if ctx.quit_requested.get() {
        return Ok(());
    }

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
pub(crate) fn absorb_target_into_source(
    src: &Utf8Path,
    dst: &Utf8Path,
    ctx: &ApplyCtx<'_>,
) -> Result<()> {
    info!("absorb: {dst} → {src}");
    backup_existing(src, ctx.backup_root, /* is_dir */ false)?;
    std::fs::copy(dst, src)?;
    link::unlink(dst)?;
    link::link_file(src, dst, ctx.file_mode)?;
    Ok(())
}

/// Inverse of `absorb_target_into_source`: keep source's content,
/// throw away target's diverged content (after backing it up), and
/// re-link target so it once again reflects source. Used when the
/// user picks `[o]verwrite` at the anomaly prompt — i.e. they edited
/// source intentionally and want the target updated to match.
pub(crate) fn overwrite_source_into_target(
    src: &Utf8Path,
    dst: &Utf8Path,
    ctx: &ApplyCtx<'_>,
) -> Result<()> {
    info!("overwrite: {src} → {dst}");
    backup_existing(dst, ctx.backup_root, /* is_dir */ false)?;
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
        Ask => match prompt_anomaly(ctx, src, dst, reason)? {
            AnomalyChoice::Absorb => absorb_target_into_source(src, dst, ctx),
            AnomalyChoice::Overwrite => overwrite_source_into_target(src, dst, ctx),
            AnomalyChoice::Skip => {
                warn!("anomaly skipped by user: {dst}");
                Ok(())
            }
            AnomalyChoice::Quit => {
                warn!("anomaly: user requested quit; stopping apply at {dst}");
                ctx.quit_requested.set(true);
                Ok(())
            }
        },
    }
}

/// Multi-choice TTY prompt for an anomaly.
///
/// Replaces the old binary y/N "absorb?" prompt with chezmoi-style
/// per-direction options plus uppercase "all-remaining" variants. The
/// caller is responsible for performing the chosen action; this
/// function only resolves the user's intent.
///
/// Sticky behaviour: if a prior prompt selected an `[A]/[O]/[S]` "all"
/// option, that choice short-circuits subsequent prompts via
/// `ctx.sticky_anomaly`. `[q]uit` flips `ctx.quit_requested` so the
/// walker stops calling per-entry link ops.
///
/// Off-TTY: returns `Skip` immediately (caller logs the downgrade) —
/// matches the previous "non-TTY ask = skip" behaviour. Quit is not
/// possible without a TTY because there is nothing to interact with.
pub(crate) fn prompt_anomaly(
    ctx: &ApplyCtx<'_>,
    src: &Utf8Path,
    dst: &Utf8Path,
    reason: &str,
) -> Result<AnomalyChoice> {
    // If a previous prompt selected `[q]uit`, every nested call (e.g.
    // remaining file conflicts inside an in-flight dir merge) returns
    // `Quit` immediately so we don't ask again, log redundant warnings,
    // or block on stdin during teardown.
    if ctx.quit_requested.get() {
        return Ok(AnomalyChoice::Quit);
    }
    if let Some(c) = ctx.sticky_anomaly.get() {
        return Ok(c);
    }

    use std::io::IsTerminal;
    use std::io::Write as _;
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(AnomalyChoice::Skip);
    }

    eprintln!();
    eprintln!("anomaly: {reason}");
    eprintln!("  src: {src}");
    eprintln!("  dst: {dst}");
    print_absorb_diff(src, dst);

    loop {
        eprintln!("  [a/A] absorb     target → source   (this / all remaining)");
        eprintln!("  [o/O] overwrite  source → target   (this / all remaining)");
        eprintln!("  [s/S] skip       leave as-is       (this / all remaining)");
        eprintln!("  [d]   diff       re-show the diff");
        eprintln!("  [q]   quit       skip this and stop apply");
        eprint!("choice [s]: ");
        std::io::stderr().flush().ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let trimmed = input.trim();
        // `y` / `n` are kept as aliases for the previous y/N prompt so
        // muscle memory keeps working: `y` = "yes, absorb" (target →
        // source), `n` = "no, leave it" (skip).
        let choice = match trimmed {
            "" | "s" | "n" => AnomalyChoice::Skip,
            "a" | "y" => AnomalyChoice::Absorb,
            "o" => AnomalyChoice::Overwrite,
            "q" => AnomalyChoice::Quit,
            "A" => {
                ctx.sticky_anomaly.set(Some(AnomalyChoice::Absorb));
                AnomalyChoice::Absorb
            }
            "O" => {
                ctx.sticky_anomaly.set(Some(AnomalyChoice::Overwrite));
                AnomalyChoice::Overwrite
            }
            "S" => {
                ctx.sticky_anomaly.set(Some(AnomalyChoice::Skip));
                AnomalyChoice::Skip
            }
            "d" => {
                print_absorb_diff(src, dst);
                continue;
            }
            other => {
                eprintln!("unknown choice: {other:?}");
                continue;
            }
        };
        return Ok(choice);
    }
}

/// Walk every diverged template and resolve each one interactively.
///
/// Caller must have already gated this on `!dry_run` — drift is only
/// surfaced via logs during dry-run so previews stay non-interactive.
/// `quit_flag` is set when the user picks `[q]uit` so `apply` can
/// short-circuit the link pass.
///
/// Sticky `[O]` / `[S]` "all remaining" choices short-circuit
/// subsequent prompts within this call.
fn resolve_render_drift(report: &render::RenderReport, quit_flag: &Cell<bool>) -> Result<()> {
    let sticky: Cell<Option<RenderDriftChoice>> = Cell::new(None);

    for entry in &report.diverged {
        if quit_flag.get() {
            // `apply` already logs "user quit … skipping link pass" once
            // when it sees `render_quit`; per-entry warns here would just
            // multiply that message by the number of remaining drifts.
            break;
        }

        let choice = match sticky.get() {
            Some(c) => c,
            None => prompt_render_drift(entry, &sticky, quit_flag)?,
        };

        match choice {
            RenderDriftChoice::Overwrite => {
                info!(
                    "render overwrite: {} → {}",
                    entry.tera_path, entry.rendered_path
                );
                if let Some(parent) = entry.rendered_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&entry.rendered_path, &entry.fresh_body)
                    .with_context(|| format!("writing fresh render to {}", entry.rendered_path))?;
            }
            RenderDriftChoice::Skip => {
                warn!(
                    "render drift skipped by user: {} (rendered file left as-is)",
                    entry.rendered_path
                );
            }
            RenderDriftChoice::Quit => {
                warn!("render drift quit: leaving {} as-is", entry.rendered_path);
            }
        }
    }

    Ok(())
}

/// Multi-choice TTY prompt for a render-drift entry.
///
/// Mirrors `prompt_anomaly`'s shape but with one fewer direction —
/// see `RenderDriftChoice` for why `[a]bsorb` is omitted. mtime is
/// used to pick the recommended default:
///   - `.tera` newer than rendered → `[o]verwrite` (user just edited
///     the template)
///   - otherwise → `[s]kip` (rendered file may carry a target-side
///     edit, don't clobber)
///
/// Off-TTY: returns `Skip` immediately (matches `prompt_anomaly`).
fn prompt_render_drift(
    entry: &render::DivergedEntry,
    sticky: &Cell<Option<RenderDriftChoice>>,
    quit_flag: &Cell<bool>,
) -> Result<RenderDriftChoice> {
    use std::io::IsTerminal;
    use std::io::Write as _;
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(RenderDriftChoice::Skip);
    }

    let default = match (entry.tera_mtime, entry.rendered_mtime) {
        (Some(t), Some(r)) if t > r => RenderDriftChoice::Overwrite,
        _ => RenderDriftChoice::Skip,
    };
    let default_label = match default {
        RenderDriftChoice::Overwrite => "o",
        _ => "s",
    };

    eprintln!();
    eprintln!("render drift: on-disk rendered file diverged from .tera output");
    eprintln!("  src (.tera):    {}", entry.tera_path);
    eprintln!("  dst (rendered): {}", entry.rendered_path);
    print_render_drift_diff(entry);

    loop {
        eprintln!("  [o/O] overwrite  .tera output → rendered   (this / all remaining)");
        eprintln!("  [s/S] skip       leave as-is                (this / all remaining)");
        eprintln!("  [d]   diff       re-show the diff");
        eprintln!("  [q]   quit       skip this and stop apply");
        eprint!("choice [{default_label}]: ");
        std::io::stderr().flush().ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let trimmed = input.trim();
        let choice = match trimmed {
            "" => default,
            "s" | "n" => RenderDriftChoice::Skip,
            "o" | "y" => RenderDriftChoice::Overwrite,
            "q" => {
                quit_flag.set(true);
                RenderDriftChoice::Quit
            }
            "O" => {
                sticky.set(Some(RenderDriftChoice::Overwrite));
                RenderDriftChoice::Overwrite
            }
            "S" => {
                sticky.set(Some(RenderDriftChoice::Skip));
                RenderDriftChoice::Skip
            }
            "d" => {
                print_render_drift_diff(entry);
                continue;
            }
            other => {
                eprintln!("unknown choice: {other:?}");
                continue;
            }
        };
        return Ok(choice);
    }
}

/// Render-drift counterpart of `print_absorb_diff`. The "src" side is
/// in-memory (the fresh template output) so we can't reuse the file→file
/// helper directly — we read the on-disk rendered file and diff it
/// against `entry.fresh_body`.
fn print_render_drift_diff(entry: &render::DivergedEntry) {
    use owo_colors::OwoColorize as _;
    use std::io::IsTerminal;

    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();

    eprintln!();
    if color {
        eprintln!(
            "{}  {}  {}",
            "── unified diff ──".bold(),
            "[-] rendered (on disk)".red().bold(),
            "[+] fresh (.tera output)".green().bold()
        );
        eprintln!("  {} {}", "[-] rendered:".red(), entry.rendered_path);
        eprintln!("  {} {}", "[+] .tera:   ".green(), entry.tera_path);
    } else {
        eprintln!("── unified diff ──  [-] rendered (on disk)   [+] fresh (.tera output)");
        eprintln!("  [-] rendered: {}", entry.rendered_path);
        eprintln!("  [+] .tera:    {}", entry.tera_path);
    }
    eprintln!();

    // Use the shared text/binary classifier so a non-UTF-8 rendered file
    // bails the diff cleanly instead of leaking a raw read error — same
    // behaviour as `print_absorb_diff`.
    let rendered = match read_text_for_diff(&entry.rendered_path) {
        DiffSide::Text(s) => s,
        DiffSide::Binary => {
            eprintln!("(binary file or non-UTF-8 content — diff skipped)");
            eprintln!();
            return;
        }
    };

    let diff = similar::TextDiff::from_lines(rendered.as_str(), entry.fresh_body.as_str());
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        let header = hunk.header().to_string();
        if color {
            eprintln!("{}", header.cyan());
        } else {
            eprintln!("{header}");
        }
        for change in hunk.iter_changes() {
            let line = change.value();
            let line = line.strip_suffix('\n').unwrap_or(line);
            match change.tag() {
                similar::ChangeTag::Delete => {
                    if color {
                        eprintln!("{} {}", "-".red().bold(), line.red());
                    } else {
                        eprintln!("- {line}");
                    }
                }
                similar::ChangeTag::Insert => {
                    if color {
                        eprintln!("{} {}", "+".green().bold(), line.green());
                    } else {
                        eprintln!("+ {line}");
                    }
                }
                similar::ChangeTag::Equal => {
                    if color {
                        eprintln!("  {}", line.dimmed());
                    } else {
                        eprintln!("  {line}");
                    }
                }
            }
        }
    }
    eprintln!();
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

    if ctx.quit_requested.get() {
        return Ok(());
    }

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
        RelinkOnly => {
            // For dirs the classifier doesn't currently produce
            // `RelinkOnly` (only InSync / NeedsConfirm), but handle it
            // for symmetry with the file path: contents already match,
            // so just swap the target for a junction to source.
            info!("relink dir: {src} → {dst}");
            remove_dir_link_or_real(dst)?;
            link::link_dir(src, dst, ctx.dir_mode)?;
            Ok(())
        }
        AutoAbsorb | NeedsConfirm => {
            // Reaching `link_dir_with_backup` means we're acting on a
            // `.yuilink` marker (or a `[[mount.entry]]` whose `src` is a
            // directory) — the user has explicitly opted into
            // "this whole subtree is target-as-truth". A dir-level
            // NeedsConfirm here is therefore *not* the same kind of
            // anomaly that file-level NeedsConfirm represents (a single
            // file the user edited and source got newer); it's just
            // "source and target dirs are different inodes" — the
            // marker already authorised us to merge.
            //
            // Per-file content conflicts *inside* the merge are still
            // a real concern (target has X, source has X with
            // different content). Those are surfaced from inside the
            // merge itself — see `merge_dir_target_into_source`'s
            // file-level dispatch — so the outer-dir decision falls
            // straight through to absorb.
            //
            // The `auto` / `require_clean_git` knobs still gate, so
            // turning them off restores the prompt before any
            // whole-dir absorb.
            if !ctx.config.absorb.auto {
                return handle_anomaly_dir(
                    src,
                    dst,
                    ctx,
                    "absorb.auto = false; treating divergence as anomaly",
                );
            }
            if ctx.config.absorb.require_clean_git && !source_repo_is_clean(ctx.source) {
                return handle_anomaly_dir(
                    src,
                    dst,
                    ctx,
                    "source repo is dirty; deferring auto-absorb",
                );
            }
            absorb_target_dir_into_source(src, dst, ctx)
        }
    }
}

/// `link::unlink` with a documented fallback for the chezmoi-migration
/// shape: target is a real (non-link) directory packed with files. The
/// caller is responsible for ensuring the target's prior content is
/// preserved (in `.yui/backup/...` or because we just merged it into
/// source) before reaching here.
///
/// Anything other than the "non-empty regular dir" case — permission
/// denied, target gone, target now a junction or symlink — propagates
/// rather than being silently coerced into `remove_dir_all`.
fn remove_dir_link_or_real(dst: &Utf8Path) -> Result<()> {
    if let Err(unlink_err) = link::unlink(dst) {
        let meta = std::fs::symlink_metadata(dst)
            .with_context(|| format!("stat {dst} after link::unlink failed: {unlink_err}"))?;
        let ft = meta.file_type();
        if ft.is_dir() && !ft.is_symlink() {
            std::fs::remove_dir_all(dst).with_context(|| {
                format!(
                    "remove_dir_all({dst}) after link::unlink failed: \
                     {unlink_err}"
                )
            })?;
        } else {
            return Err(unlink_err).with_context(|| format!("unlink({dst}) before relink"));
        }
    }
    Ok(())
}

/// Recursively merge target's files into source: target wins on file
/// conflicts, source-only files are preserved, sub-dirs are created
/// in source as needed. Non-regular entries (symlinks / junctions /
/// device files) are skipped with a warning — copying their content
/// is ill-defined and following them risks looping into target via
/// some chain back to source.
///
/// Mirrors the file-level "AutoAbsorb backs up source, copies target's
/// content into source before relinking" semantic for whole dirs.
fn merge_dir_target_into_source(
    target: &Utf8Path,
    source: &Utf8Path,
    ctx: &ApplyCtx<'_>,
) -> Result<()> {
    for entry in std::fs::read_dir(target)? {
        // If the user picked `[q]uit` at a previous file-conflict
        // prompt, every remaining entry in this dir merge becomes a
        // no-op. The enclosing `absorb_target_dir_into_source` checks
        // the same flag *after* the merge returns and skips the
        // teardown/relink that would otherwise complete the absorb
        // the user just asked us to abandon.
        if ctx.quit_requested.get() {
            return Ok(());
        }
        let entry = entry?;
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        let target_path = target.join(name);
        let source_path = source.join(name);
        let ft = entry.file_type()?;

        if ft.is_dir() && !ft.is_symlink() {
            // Target is a real dir. If source has a non-dir entry at
            // the same name (regular file, symlink, junction), it
            // would block `create_dir_all` and the recursive merge.
            // Honor target-wins by clearing the conflicting source
            // entry first.
            if let Ok(src_meta) = std::fs::symlink_metadata(&source_path) {
                let sft = src_meta.file_type();
                if !sft.is_dir() || sft.is_symlink() {
                    link::unlink(&source_path).with_context(|| {
                        format!("remove conflicting source entry before dir merge: {source_path}")
                    })?;
                }
            }
            if !source_path.exists() {
                std::fs::create_dir_all(&source_path).with_context(|| {
                    format!("create_dir_all({source_path}) during target→source merge")
                })?;
            }
            merge_dir_target_into_source(&target_path, &source_path, ctx)?;
        } else if ft.is_file() {
            // Target is a regular file. Symmetrical handling: if
            // source has a directory or symlink at the same name,
            // tear it down first so the file copy can land.
            if let Ok(src_meta) = std::fs::symlink_metadata(&source_path) {
                let sft = src_meta.file_type();
                if sft.is_dir() && !sft.is_symlink() {
                    remove_dir_link_or_real(&source_path).with_context(|| {
                        format!("remove conflicting source dir before file merge: {source_path}")
                    })?;
                } else if sft.is_symlink() {
                    link::unlink(&source_path).with_context(|| {
                        format!(
                            "remove conflicting source symlink before file merge: {source_path}"
                        )
                    })?;
                }
            }
            if let Some(parent) = source_path.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            // If both sides are now regular files at the same path, run
            // the file-level absorb classifier so this single overlap
            // is resolved against `[absorb]` policy (auto / skip /
            // force / ask) instead of being silently overwritten. The
            // dir-level marker provides consent for the *whole-tree*
            // merge, but a per-file content collision where the
            // source side is *newer* is still a legitimate anomaly
            // worth surfacing.
            //
            // Source-only files were already preserved by virtue of
            // the merge not visiting them. Target-only files (where
            // `source_path` doesn't exist) skip the classifier and go
            // straight to copy below.
            if source_path.is_file() {
                merge_resolve_file_conflict(&target_path, &source_path, ctx)?;
            } else {
                std::fs::copy(&target_path, &source_path)
                    .with_context(|| format!("copy({target_path} → {source_path}) during merge"))?;
            }
        } else {
            warn!(
                "merge: skipping non-regular entry {target_path} \
                 (symlink / junction / special — content not copied)"
            );
        }
    }
    Ok(())
}

/// Per-file conflict resolution inside the dir merge. Both
/// `target_path` and `source_path` exist as regular files — run the
/// absorb classifier on the pair and route to the matching policy:
///
/// - `InSync` / `RelinkOnly` → no-op (contents already match)
/// - `AutoAbsorb` (target newer + diff) → copy target → source,
///   target-wins per the AutoAbsorb contract.
/// - `NeedsConfirm` (source newer + diff, the genuine anomaly) →
///   `[absorb] on_anomaly` dispatch:
///     - `skip` → leave source alone, target's version is dropped
///       (after the outer junction, target ends up with source's content)
///     - `force` → copy target → source (target wins anyway)
///     - `ask` → TTY prompt with diff; downgrade to skip off-TTY
fn merge_resolve_file_conflict(
    target_path: &Utf8Path,
    source_path: &Utf8Path,
    ctx: &ApplyCtx<'_>,
) -> Result<()> {
    use absorb::AbsorbDecision::*;
    let decision = absorb::classify(source_path, target_path)?;
    match decision {
        InSync | RelinkOnly => Ok(()),
        AutoAbsorb => {
            std::fs::copy(target_path, source_path).with_context(|| {
                format!("copy({target_path} → {source_path}) during merge AutoAbsorb")
            })?;
            Ok(())
        }
        Restore => {
            // `Restore` is the classifier's "target is missing" arm.
            // We only enter this function after the merge loop saw
            // `target_path` as a regular file in the read_dir
            // iteration, and the caller guards on `source_path.is_file()`
            // — both exist by construction, so this branch is
            // unreachable.
            unreachable!(
                "merge_resolve_file_conflict reached with both files present, \
                 but classify returned Restore (target {target_path} / source {source_path})"
            )
        }
        NeedsConfirm => {
            use crate::config::AnomalyAction::*;
            match ctx.config.absorb.on_anomaly {
                Skip => {
                    warn!(
                        "merge anomaly skip: {target_path} (source-newer / content drift) \
                         — keeping source version, target version dropped"
                    );
                    Ok(())
                }
                Force => {
                    warn!(
                        "merge anomaly force: {target_path} \
                         (source-newer / content drift) — overwriting source"
                    );
                    std::fs::copy(target_path, source_path)?;
                    Ok(())
                }
                Ask => {
                    let choice = prompt_anomaly(
                        ctx,
                        source_path,
                        target_path,
                        "merge: file content differs and source is newer",
                    )?;
                    match choice {
                        AnomalyChoice::Absorb => {
                            std::fs::copy(target_path, source_path)?;
                            Ok(())
                        }
                        AnomalyChoice::Overwrite => {
                            // Preserve target's diverged content before
                            // we clobber it with source's. The enclosing
                            // dir absorb later removes target's tree
                            // wholesale, so without this backup the
                            // pre-overwrite state is unrecoverable.
                            // Mirrors the file-level overwrite path.
                            backup_existing(target_path, ctx.backup_root, /* is_dir */ false)?;
                            std::fs::copy(source_path, target_path)?;
                            Ok(())
                        }
                        AnomalyChoice::Skip => {
                            warn!("merge: kept source version by user choice: {source_path}");
                            Ok(())
                        }
                        AnomalyChoice::Quit => {
                            warn!("merge: user requested quit; stopping at {target_path}");
                            ctx.quit_requested.set(true);
                            Ok(())
                        }
                    }
                }
            }
        }
    }
}

/// Back up source-side, merge target's content into source (target
/// wins on conflict), then replace target with a junction to source.
/// "Target wins" — yui's core philosophy, generalised from the file
/// path to whole directories so a chezmoi-style migrated `~/.config/`
/// keeps every file the user actually had instead of stranding most
/// of them in `.yui/backup/...`.
pub(crate) fn absorb_target_dir_into_source(
    src: &Utf8Path,
    dst: &Utf8Path,
    ctx: &ApplyCtx<'_>,
) -> Result<()> {
    info!("absorb dir: {dst} → {src}");
    backup_existing(src, ctx.backup_root, /* is_dir */ true)?;
    merge_dir_target_into_source(dst, src, ctx)?;
    // If the user picked `[q]uit` at a prompt during the merge, do not
    // proceed with the teardown/relink that would finish the absorb
    // the user just asked us to abandon. `dst` keeps its (partially
    // populated) tree intact, source has whatever content was already
    // merged before the quit, and the source-side backup taken at the
    // top of this function is the recovery anchor.
    if ctx.quit_requested.get() {
        warn!(
            "absorb dir interrupted by user quit: {dst} \
             — leaving target tree intact; source backup at {}",
            ctx.backup_root
        );
        return Ok(());
    }
    // Source now carries every regular file from target. Tear down the
    // original target dir and re-expose source via a junction.
    remove_dir_link_or_real(dst)?;
    link::link_dir(src, dst, ctx.dir_mode)?;
    Ok(())
}

/// Inverse of `absorb_target_dir_into_source`: keep source's dir
/// content as-is, back up target's diverged content, then re-expose
/// source via a junction at the target path. Used when the user
/// picks `[o]verwrite` for a dir-level anomaly.
fn overwrite_source_dir_into_target(
    src: &Utf8Path,
    dst: &Utf8Path,
    ctx: &ApplyCtx<'_>,
) -> Result<()> {
    info!("overwrite dir: {src} → {dst}");
    backup_existing(dst, ctx.backup_root, /* is_dir */ true)?;
    remove_dir_link_or_real(dst)?;
    link::link_dir(src, dst, ctx.dir_mode)?;
    Ok(())
}

/// Dir-level counterpart to `handle_anomaly`. Same `[absorb] on_anomaly`
/// dispatch — `skip` warns and walks away, `force` absorbs anyway,
/// `ask` prompts on a TTY (downgraded to skip off-TTY).
fn handle_anomaly_dir(
    src: &Utf8Path,
    dst: &Utf8Path,
    ctx: &ApplyCtx<'_>,
    reason: &str,
) -> Result<()> {
    use crate::config::AnomalyAction::*;
    match ctx.config.absorb.on_anomaly {
        Skip => {
            warn!("anomaly skip dir: {dst} ({reason})");
            Ok(())
        }
        Force => {
            warn!(
                "anomaly force dir: {dst} ({reason}) \
                 — absorbing target into source"
            );
            absorb_target_dir_into_source(src, dst, ctx)
        }
        Ask => match prompt_anomaly(ctx, src, dst, reason)? {
            AnomalyChoice::Absorb => absorb_target_dir_into_source(src, dst, ctx),
            AnomalyChoice::Overwrite => overwrite_source_dir_into_target(src, dst, ctx),
            AnomalyChoice::Skip => {
                warn!("anomaly skipped by user: {dst}");
                Ok(())
            }
            AnomalyChoice::Quit => {
                warn!("anomaly dir: user requested quit; stopping apply at {dst}");
                ctx.quit_requested.set(true);
                Ok(())
            }
        },
    }
}

fn backup_existing(target: &Utf8Path, backup_root: &Utf8Path, is_dir: bool) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
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
