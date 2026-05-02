//! Command implementations.
//!
//! Each `Command` variant in `cli.rs` calls one of these.

use std::cell::Cell;
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
use crate::secret;
use crate::template;
use crate::vars::YuiVars;
use crate::vault;
use crate::{absorb, backup, paths};

// NOTE: `owo_colors::OwoColorize` is intentionally NOT imported at module
// scope — its blanket impl shadows inherent methods of unrelated types
// (e.g. `ignore::WalkBuilder::hidden(bool)` collides with
// `OwoColorize::hidden(&self)`). Each print function imports the trait
// locally with `use owo_colors::OwoColorize as _;`.

pub fn init(source: Option<Utf8PathBuf>, git_hooks: bool) -> Result<()> {
    let dir = match source {
        Some(s) => absolutize(&s)?,
        None => current_dir_utf8()?,
    };
    std::fs::create_dir_all(&dir)?;
    let config_path = dir.join("config.toml");
    let scaffolded = if !config_path.exists() {
        std::fs::write(&config_path, SKELETON_CONFIG)?;
        info!("initialized yui source repo at {dir}");
        info!("created: {config_path}");
        true
    } else if git_hooks {
        // Existing repo + hooks-only invocation: just install the
        // hooks. Don't bail like we used to — a user who already has
        // a populated dotfiles repo shouldn't need to delete
        // config.toml to opt into the render-drift hooks.
        info!(
            "config.toml already exists at {config_path} \
             — skipping scaffold, installing git hooks only"
        );
        false
    } else {
        anyhow::bail!("config.toml already exists at {config_path}");
    };

    // .gitignore upkeep is `init`'s responsibility — running it
    // again on an existing repo (e.g. for a hooks-only install)
    // should still backfill the yui-required ignore lines if the
    // .gitignore has drifted. The rendered-template section is
    // separately maintained by `apply`'s render flow, so we only
    // touch the state / backup / config.local entries here.
    ensure_gitignore_yui_entries(&dir)?;

    if git_hooks {
        install_git_hooks(&dir)?;
    }
    if scaffolded {
        info!("next: edit config.toml, then run `yui apply`");
    }
    Ok(())
}

/// .gitignore lines yui needs every dotfiles repo to carry. Anything
/// the render flow auto-manages (the `# >>> yui rendered ... <<<`
/// section) lives there; what `init` owns is the per-machine state +
/// backup pile + the `config.local.toml` carve-out.
const YUI_REQUIRED_GITIGNORE: &[&str] = &[
    "/.yui/state.json",
    "/.yui/state.json.tmp",
    "/.yui/backup/",
    "config.local.toml",
];

/// Ensure each `YUI_REQUIRED_GITIGNORE` line is present in the repo's
/// `.gitignore`. Creates the file with the full skeleton when it's
/// missing entirely, and appends only the missing entries (in a
/// labelled section) when it already exists. Idempotent — re-running
/// `init` is a no-op once the entries are in place.
fn ensure_gitignore_yui_entries(dir: &Utf8Path) -> Result<()> {
    let path = dir.join(".gitignore");
    if !path.exists() {
        std::fs::write(&path, SKELETON_GITIGNORE)?;
        info!("created: {path}");
        return Ok(());
    }
    let existing = std::fs::read_to_string(&path)?;
    let missing: Vec<&str> = YUI_REQUIRED_GITIGNORE
        .iter()
        .copied()
        .filter(|entry| !existing.lines().any(|line| line.trim() == *entry))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str("# yui per-machine state and backups (added by `yui init`).\n");
    for entry in &missing {
        next.push_str(entry);
        next.push('\n');
    }
    std::fs::write(&path, next)?;
    info!(
        "updated .gitignore: appended {} yui entr{} ({})",
        missing.len(),
        if missing.len() == 1 { "y" } else { "ies" },
        missing.join(", ")
    );
    Ok(())
}

/// Install yui's render-drift hooks into the source repo's
/// `.git/hooks/`. Both pre-commit and pre-push run `yui render --check`
/// — pre-commit catches the easy case (you forgot to `apply` before
/// committing), pre-push is the safety net that catches anything a
/// bypassed pre-commit (or a `git commit --no-verify`) let slip
/// through.
///
/// Asks git for the hooks directory via `rev-parse --git-path hooks`
/// so `core.hooksPath` (configured globally or per-repo to redirect
/// hooks elsewhere) is honoured, and worktrees / bare repos / GIT_DIR
/// overrides come along for the ride. Refuses to overwrite existing
/// hooks — the user has to delete them first if they want yui to
/// manage that slot.
fn install_git_hooks(source: &Utf8Path) -> Result<()> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(source.as_std_path())
        .output()
        .with_context(|| format!("git rev-parse --git-path hooks in {source}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "--git-hooks: {source} doesn't look like a git repo \
             (run `git init` first). git: {}",
            stderr.trim()
        );
    }
    let raw = String::from_utf8(out.stdout)?;
    let hooks_dir = {
        let p = Utf8PathBuf::from(raw.trim());
        if p.is_absolute() { p } else { source.join(p) }
    };
    std::fs::create_dir_all(&hooks_dir).with_context(|| format!("mkdir -p {hooks_dir}"))?;

    for (name, body) in [("pre-commit", PRE_COMMIT_HOOK), ("pre-push", PRE_PUSH_HOOK)] {
        let path = hooks_dir.join(name);
        if path.exists() {
            warn!("--git-hooks: {path} already exists — leaving it alone");
            continue;
        }
        std::fs::write(&path, body).with_context(|| format!("write hook {path}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms)?;
        }
        info!("installed: {path}");
    }
    Ok(())
}

const PRE_COMMIT_HOOK: &str = r#"#!/bin/sh
# Installed by `yui init --git-hooks`.
# Reject the commit if any `*.tera` template would render to something
# that diverges from the rendered output staged alongside it. Run
# `yui apply` (or `yui render`) to refresh and re-commit.
exec yui render --check
"#;

const PRE_PUSH_HOOK: &str = r#"#!/bin/sh
# Installed by `yui init --git-hooks`.
# Same render-drift check as pre-commit, mirrored on push so a
# `--no-verify` commit doesn't sneak diverged state to the remote.
exec yui render --check
"#;

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
    let render_report = render::render_all(&source, &config, &yui, dry_run)?;
    log_render_report(&render_report);
    if render_report.has_drift() {
        anyhow::bail!(
            "render drift detected ({} file(s)); reflect target edits back into the .tera before re-running apply",
            render_report.diverged.len()
        );
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

    // Nested `.yuiignore` stack — push on dir entry, pop on exit.
    // Seed with the source-root layer so root-level rules apply from
    // the start without `walk_and_link` having to special-case it.
    let mut yuiignore = paths::YuiIgnoreStack::new();
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
enum AnomalyChoice {
    /// target → source (yui's default, "target is truth").
    Absorb,
    /// source → target (user-edited source wins, target updated).
    Overwrite,
    /// Leave both as-is for now.
    Skip,
    /// Skip this entry and stop walking remaining entries.
    Quit,
}

struct ApplyCtx<'a> {
    config: &'a Config,
    /// Source repo root — needed for git-clean checks during absorb.
    source: &'a Utf8Path,
    file_mode: EffectiveFileMode,
    dir_mode: EffectiveDirMode,
    backup_root: &'a Utf8Path,
    dry_run: bool,
    /// Sticky decision from a previous "all" prompt. When set, every
    /// subsequent anomaly applies this choice without prompting.
    sticky_anomaly: Cell<Option<AnomalyChoice>>,
    /// Set by the `[q]uit` choice. The walker checks this at the top
    /// of every link op and short-circuits to a no-op so apply exits
    /// cleanly without further prompts.
    quit_requested: Cell<bool>,
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
        // .yuiignore filtering happens in `source_walker` via
        // `add_custom_ignore_filename` — markers under ignored
        // subtrees never reach here.
        let spec = match marker::read_spec(&dir_utf8, marker_filename)? {
            Some(s) => s,
            None => continue,
        };
        let MarkerSpec::Explicit { links } = spec else {
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
            // File-level entry (`[[link]] src = "<filename>"`) targets a
            // single file inside the marker dir; show that file path
            // instead of the bare dir so `yui list` makes the scope
            // obvious at a glance.
            let src_display = match &link.src {
                Some(filename) => rel.join(filename),
                None => rel.clone(),
            };
            items.push(ListItem {
                src: src_display,
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

/// `yui secret init [--comment TEXT]` — generate an age X25519
/// keypair on this machine, write the secret to the configured
/// identity path, and append the public key to
/// `$DOTFILES/config.toml` `[secrets] recipients`.
///
/// `config.toml` is the *committed* config (not the per-machine
/// `config.local.toml`). That's load-bearing for multi-machine
/// use: `recipients` is the public-key list every `*.age`
/// encryption wraps to, so machine B needs to see machine A's
/// public key after A runs `yui secret init`. Public keys are
/// safe to commit — the ciphertext only opens with the matching
/// secret, which never leaves the machine that generated it.
///
/// ## Migrating from yui ≤ v0.7.13
///
/// Older versions wrote the recipient into `config.local.toml`
/// (gitignored), which silently broke multi-machine use. If you
/// ran `yui secret init` against an earlier yui:
///
/// 1. Open `$DOTFILES/config.local.toml` and locate the
///    `[secrets] recipients = [...]` block.
/// 2. Cut it and paste it into `$DOTFILES/config.toml`.
/// 3. `git add config.toml && git commit && git push`.
/// 4. On every other machine: `git pull && yui apply` once.
///
/// Subsequent `yui secret init` (e.g. on a new machine) appends
/// directly to `config.toml` — no manual move needed.
pub fn secret_init(source: Option<Utf8PathBuf>, comment: Option<String>) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    // 1. Resolve identity path (default: ~/.config/yui/age.txt).
    let identity_path = paths::expand_tilde(&config.secrets.identity);
    if identity_path.exists() {
        anyhow::bail!(
            "identity file already exists at {identity_path}; \
             refusing to overwrite. Delete it first if you really \
             mean to start fresh (you'll lose access to existing \
             .age files encrypted to its public key)."
        );
    }

    // 2. Generate the keypair + serialise the identity file with
    //    the same header age-keygen uses, so the file is
    //    interoperable with the standalone CLI tools.
    let (secret, public) = secret::generate_x25519_keypair();
    let now = jiff::Zoned::now().to_string();
    let body = format!(
        "# created: {now}\n\
         # public key: {public}\n\
         {secret}\n"
    );
    // 0600 on Unix so other local users can't read the X25519
    // secret. PR #60 review by coderabbitai.
    secret::write_private_file(&identity_path, body.as_bytes())?;
    info!("wrote identity file: {identity_path}");

    // 3. Append the public key to `[secrets] recipients` in the
    //    committed `config.toml`. Recipients are public — the
    //    other machines need to see this entry to encrypt new
    //    `*.age` files for the user who just ran init.
    let config_path = source.join("config.toml");
    let comment = comment.unwrap_or_else(|| format!("{} {}", yui.host, yui.user));
    let entry_comment = format!("{comment} — added by `yui secret init` on {now}");
    let config_existing = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => anyhow::bail!("read {config_path}: {e}"),
    };
    let updated_config = append_recipient_to_config(&config_existing, &entry_comment, &public)?;
    std::fs::write(&config_path, updated_config)?;
    info!("appended public key to {config_path}");
    println!();
    println!("  age identity:  {identity_path}");
    println!("  public key:    {public}");
    println!();
    println!(
        "  Next: encrypt a file with `yui secret encrypt <path>`. \
         The plaintext sibling will be auto-decrypted on every `yui apply`."
    );
    Ok(())
}

/// Append a recipient entry to the user's `config.toml`.
///
/// Uses `toml_edit` to parse the file into an in-memory document
/// tree, modify the `[secrets].recipients` array, then serialise
/// back. This preserves user comments / spacing / table ordering,
/// and survives quirky inputs (other tables after `[secrets]`,
/// trailing comments, multi-line arrays, etc.) — string-pasting
/// the same shape used to land tokens in the wrong place when the
/// file's layout deviated from the most common case. (Caught in
/// PR #57 review by gemini-code-assist.)
///
/// Returns the file unchanged when the public key is already in
/// the recipients list (idempotent re-init).
fn append_recipient_to_config(existing: &str, comment: &str, public: &str) -> Result<String> {
    use toml_edit::{Array, DocumentMut, Item, Table, Value};

    let mut doc: DocumentMut = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing
            .parse()
            .map_err(|e| anyhow::anyhow!("config.toml is not valid TOML: {e}"))?
    };

    // Make sure `[secrets]` exists as a table.
    if !doc.contains_key("secrets") {
        let mut t = Table::new();
        t.set_implicit(false);
        doc.insert("secrets", Item::Table(t));
    }
    let secrets = doc["secrets"].as_table_mut().ok_or_else(|| {
        anyhow::anyhow!("[secrets] in config.toml is not a table — refusing to clobber")
    })?;

    // Make sure `recipients` is an array.
    if !secrets.contains_key("recipients") {
        secrets.insert("recipients", Item::Value(Value::Array(Array::new())));
    }
    let recipients = secrets["recipients"]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("[secrets].recipients is not an array"))?;

    // Idempotent: if the public key already appears, we're done.
    let already_present = recipients.iter().any(|v| v.as_str() == Some(public));
    if already_present {
        return Ok(doc.to_string());
    }

    // Append the new entry with a leading-comment decor block so
    // the user can tell which key belongs to which machine just by
    // reading the file.
    let mut value = Value::from(public);
    let prefix = format!("\n  # {comment}\n  ");
    *value.decor_mut() = toml_edit::Decor::new(prefix, "");
    recipients.push_formatted(value);
    // Force the array onto multiple lines so the comments above
    // entries actually have a place to live (a single-line array
    // can't carry per-element comments).
    recipients.set_trailing("\n");
    recipients.set_trailing_comma(true);

    Ok(doc.to_string())
}

/// `yui secret encrypt <path> [--force] [--rm-plaintext]` — encrypt
/// a plaintext file to every recipient in `[secrets] recipients`
/// and write the ciphertext alongside as `<path>.age`.
pub fn secret_encrypt(
    source: Option<Utf8PathBuf>,
    path: Utf8PathBuf,
    force: bool,
    rm_plaintext: bool,
) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    if !config.secrets.enabled() {
        anyhow::bail!(
            "no recipients configured — run `yui secret init` to generate \
             a keypair, or add at least one entry to `[secrets] recipients`."
        );
    }

    // Resolve the plaintext path: absolute as-is, relative against
    // CWD (so the user can `yui secret encrypt home/.ssh/id_ed25519`
    // from inside `$DOTFILES`).
    let plaintext_path = if path.is_absolute() {
        path.clone()
    } else {
        absolutize(&path)?
    };
    if !plaintext_path.is_file() {
        anyhow::bail!("plaintext file not found: {plaintext_path}");
    }
    let cipher_path = Utf8PathBuf::from(format!("{plaintext_path}.age"));
    if cipher_path.exists() && !force {
        anyhow::bail!("{cipher_path} already exists; pass --force to overwrite");
    }

    let plaintext = std::fs::read(&plaintext_path)?;
    // Use the general parser so `[secrets].recipients` can hold
    // plugin entries (`age1yubikey1…` / `age1fido2-hmac1…` etc.)
    // alongside the X25519 ones. yui doesn't drive plugin flows
    // first-class, but a hand-written plugin recipient still gets
    // a stanza in the ciphertext — useful if a user wants their
    // YubiKey to decrypt the same `*.age` outside yui via the
    // standalone `age` CLI.
    let recipients = secret::parse_passkey_recipients(&config.secrets.recipients)?;
    let cipher = secret::encrypt_to_passkeys(&plaintext, &recipients)?;
    std::fs::write(&cipher_path, &cipher)?;
    info!("encrypted {plaintext_path} → {cipher_path}");

    if rm_plaintext {
        // Only remove plaintext when it lives under `$DOTFILES` —
        // erasing files outside the repo on a typo would be cruel.
        if plaintext_path.starts_with(&source) {
            std::fs::remove_file(&plaintext_path)?;
            info!("removed plaintext: {plaintext_path}");
        } else {
            warn!(
                "plaintext lives outside source ({plaintext_path}); \
                 skipping --rm-plaintext as a safety check"
            );
        }
    }
    Ok(())
}

/// `yui secret store [--force]` — push the X25519 identity at
/// `[secrets].identity` into the configured `[secrets.vault]`.
/// Run on a machine that already has the identity; the new
/// machine then recovers it via `yui secret unlock`.
///
/// yui doesn't drive the vault's auth flow itself — it shells
/// out to `bw` / `op`. Whatever those CLIs are configured to
/// accept (master password, biometric, passkey unlock in the
/// web vault, SSO) gates the operation.
pub fn secret_store(source: Option<Utf8PathBuf>, force: bool) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    let vault_cfg = config.secrets.vault.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "[secrets.vault] is not configured — set provider \
             (\"bitwarden\" or \"1password\") and item before \
             calling store"
        )
    })?;

    let identity_path = paths::expand_tilde(&config.secrets.identity);
    if !identity_path.is_file() {
        anyhow::bail!(
            "no X25519 identity at {identity_path}; run `yui secret init` first \
             (store needs that file's content to push to the vault)"
        );
    }
    let plaintext = std::fs::read(&identity_path)?;
    // Refuse to upload bytes that aren't actually an age identity
    // — a mistyped `[secrets].identity` path or a corrupted file
    // would otherwise stash garbage that `yui secret unlock`
    // would only fail to use later. (PR #61 review by coderabbitai.)
    secret::validate_x25519_identity_bytes(&plaintext)?;

    let vault = vault::driver(vault_cfg);
    // Verify the provider CLI is installed and authenticated
    // BEFORE reading the identity into memory + pushing — gives
    // the user an actionable hint instead of the raw `bw` /
    // `op` error from the upcoming write.
    vault.precheck()?;
    info!(
        "pushing X25519 identity to {} item {:?}",
        vault.provider_name(),
        config::VAULT_ITEM_NAME
    );
    vault.store(config::VAULT_ITEM_NAME, &plaintext, force)?;

    println!();
    println!(
        "  X25519 identity pushed to {} item {:?}",
        vault.provider_name(),
        config::VAULT_ITEM_NAME
    );
    println!("  On a new machine, run `yui secret unlock`.");
    Ok(())
}

/// `yui secret unlock` — fetch the X25519 identity from the
/// configured `[secrets.vault]` and write it to
/// `[secrets].identity`. The vault provider's CLI (`bw` / `op`)
/// handles auth — yui inherits whatever factor that CLI is
/// configured to require.
pub fn secret_unlock(source: Option<Utf8PathBuf>) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    let vault_cfg = config.secrets.vault.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "[secrets.vault] is not configured — nothing to unlock. \
             Run `yui secret init` + `yui secret store` on an existing \
             machine first, then commit + push the config."
        )
    })?;
    let identity_path = paths::expand_tilde(&config.secrets.identity);
    if identity_path.exists() {
        anyhow::bail!(
            "{identity_path} already exists — refusing to clobber a live \
             X25519 identity. Delete it first if you really mean to \
             re-unlock from scratch."
        );
    }

    let vault = vault::driver(vault_cfg);
    vault.precheck()?;
    info!(
        "fetching X25519 identity from {} item {:?}",
        vault.provider_name(),
        config::VAULT_ITEM_NAME
    );
    let plaintext = vault.fetch(config::VAULT_ITEM_NAME)?;

    // Validate before persisting — the vault could legitimately
    // hold any blob, so the fetched bytes might not actually be
    // an age identity (typo'd item name, wrong field). Bail
    // before touching `[secrets].identity` so a future apply
    // doesn't fail with a confusing "not a valid age key" error.
    secret::validate_x25519_identity_bytes(&plaintext)?;

    // 0600 on Unix — never leave the X25519 secret world-readable.
    secret::write_private_file(&identity_path, &plaintext)?;
    info!("wrote X25519 identity: {identity_path}");
    println!();
    println!("  X25519 identity restored at {identity_path}");
    println!("  Run `yui apply` next.");
    Ok(())
}

/// `yui update [--dry-run]` — pull source repo and re-apply.
///
/// Equivalent to `git -C $DOTFILES pull --ff-only && yui apply`,
/// but with the safety check that the source tree is clean first
/// (otherwise the pull could mix upstream commits with the user's
/// in-progress edits in surprising ways). Bails on a dirty source
/// rather than stashing — the user should commit consciously.
///
/// `--dry-run` only forwards to `apply --dry-run`; the pull itself
/// always runs (it's a read+merge operation, no half-state).
pub fn update(source: Option<Utf8PathBuf>, dry_run: bool) -> Result<()> {
    let source = resolve_source(source)?;
    if !crate::git::is_clean(&source)? {
        anyhow::bail!(
            "source repo {source} has uncommitted changes — \
             commit or stash before `yui update` (or run \
             `git pull` + `yui apply` manually if you know what \
             you're doing)"
        );
    }
    info!("git pull --ff-only at {source}");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(source.as_str())
        .arg("pull")
        .arg("--ff-only")
        .status()
        .map_err(|e| anyhow::anyhow!("invoking git: {e}"))?;
    if !status.success() {
        anyhow::bail!("git pull --ff-only failed at {source}");
    }
    apply(Some(source), dry_run)
}

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
fn is_repo_meta(path: &Utf8Path, source: &Utf8Path, marker_filename: &str) -> bool {
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

/// `yui diff [--icons MODE] [--no-color]` — for every drifted entry
/// (link or render), print a unified diff to stdout.
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
    let mut yuiignore = paths::YuiIgnoreStack::new();
    yuiignore.push_dir(&source)?;
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
    for rendered in &render_report.diverged {
        let tera_path = Utf8PathBuf::from(format!("{rendered}.tera"));
        report.push(StatusItem {
            src: tera_path,
            dst: rendered.clone(),
            state: StatusState::RenderDrift,
        });
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
/// rows already carry an absolute `.tera` path (built from
/// `render_report.diverged`, which the walker yields as absolute).
/// (Caught in PR #53 review by coderabbitai.)
fn resolve_diff_src(item: &StatusItem, source: &Utf8Path) -> Utf8PathBuf {
    match item.state {
        StatusState::RenderDrift => item.src.clone(),
        StatusState::Link(_) => source.join(&item.src),
    }
}

fn diff_worth_printing(state: &StatusState) -> bool {
    use absorb::AbsorbDecision::*;
    match state {
        StatusState::Link(InSync) => false,
        StatusState::Link(Restore) => false, // target missing — nothing to diff
        StatusState::Link(RelinkOnly) => false, // content identical, only metadata drift
        StatusState::Link(_) => true,
        StatusState::RenderDrift => true,
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
    //   - Link(_)     → read the source file from disk.
    let src_content = match state {
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

/// One side of a textual diff. `Binary` means the bytes weren't
/// valid UTF-8 (likely a binary file); the diff renderer surfaces
/// a one-liner instead of dumping bytes through `similar`.
/// Missing-file / permission errors collapse to `Text("")` so a
/// race during the walk doesn't bail the whole flow.
enum DiffSide {
    Text(String),
    Binary,
}

fn read_text_for_diff(p: &Utf8Path) -> DiffSide {
    match std::fs::read_to_string(p) {
        Ok(s) => DiffSide::Text(s),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => DiffSide::Binary,
        Err(_) => DiffSide::Text(String::new()),
    }
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

/// Manually absorb a single target file back into source.
///
/// Used when `apply` has skipped an anomaly (`[absorb] on_anomaly = "skip"`
/// or non-TTY ask) but the user has decided that target is right. Bypasses
/// policy + git-clean checks: this is an explicit user request.
///
/// Always prints a unified diff (source vs target) to stderr first.
/// Without `--yes`, requires interactive y/N confirmation on a TTY,
/// and refuses to act off-TTY (so a CI script can't silently
/// rewrite source). `--dry-run` shows the diff and exits.
///
/// Walks `[[mount.entry]]` and `.yuilink` overrides to find which source
/// path "owns" the given target. Errors loudly if no mount claims it.
pub fn absorb(
    source: Option<Utf8PathBuf>,
    target: Utf8PathBuf,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let source = resolve_source(source)?;
    let target = absolutize(&target)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;

    let mut engine = template::Engine::new();
    let tera_ctx = template::template_context(&yui, &config.vars);

    let src_path = match find_source_for_target(&source, &config, &target, &mut engine, &tera_ctx)?
    {
        Some(s) => s,
        None => anyhow::bail!(
            "no mount entry / .yuilink override claims target {target}; \
                 pass a path inside a known dst"
        ),
    };

    info!("source for {target}: {src_path}");

    // Show the diff before *any* action. For text files we render a
    // unified diff against `similar`; for dirs / binaries we just
    // surface a one-liner so the user knows what they're about to
    // overwrite without dumping garbage to the terminal.
    print_absorb_diff(&src_path, &target);

    if dry_run {
        info!("[dry-run] would absorb {target} → {src_path}");
        return Ok(());
    }

    if !yes {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "manual absorb refuses to run off-TTY without --yes \
                 (would silently overwrite {src_path})"
            );
        }
        if !prompt_yes_no("absorb target into source?")? {
            warn!("manual absorb cancelled by user: {target}");
            return Ok(());
        }
    }

    let backup_root = source.join(&config.backup.dir);
    let ctx = ApplyCtx {
        config: &config,
        source: &source,
        file_mode: resolve_file_mode(config.link.file_mode),
        dir_mode: resolve_dir_mode(config.link.dir_mode),
        backup_root: &backup_root,
        dry_run: false,
        sticky_anomaly: Cell::new(None),
        quit_requested: Cell::new(false),
    };

    // Manual absorb is an explicit user request — bypass `auto`,
    // `require_clean_git`, and `on_anomaly` policy entirely.
    absorb_target_into_source(&src_path, &target, &ctx)
}

/// Stderr-print a unified diff between `src` (file or dir) and `dst`
/// using `similar`. Falls back to a one-line description when one
/// side is a directory or content isn't valid UTF-8 — we'd rather
/// say "binary file differs" than spew bytes through `similar`.
fn print_absorb_diff(src: &Utf8Path, dst: &Utf8Path) {
    use owo_colors::OwoColorize as _;
    use std::io::IsTerminal;

    let color = std::io::stderr().is_terminal();

    eprintln!();
    if color {
        eprintln!(
            "{}  {}  {}",
            "── unified diff ──".bold(),
            "[-] src".red().bold(),
            "[+] dst".green().bold()
        );
        eprintln!("  {} {}", "[-] src:".red(), src);
        eprintln!("  {} {}", "[+] dst:".green(), dst);
    } else {
        eprintln!("── unified diff ──  [-] src   [+] dst");
        eprintln!("  [-] src: {src}");
        eprintln!("  [+] dst: {dst}");
    }
    eprintln!();

    if src.is_dir() || dst.is_dir() {
        eprintln!("(directory absorb — content listing skipped)");
        eprintln!();
        return;
    }
    let src_content = match read_text_for_diff(src) {
        DiffSide::Text(s) => s,
        DiffSide::Binary => {
            eprintln!("(binary file or non-UTF-8 content — diff skipped)");
            eprintln!();
            return;
        }
    };
    let dst_content = match read_text_for_diff(dst) {
        DiffSide::Text(s) => s,
        DiffSide::Binary => {
            eprintln!("(binary file or non-UTF-8 content — diff skipped)");
            eprintln!();
            return;
        }
    };

    let diff = similar::TextDiff::from_lines(&src_content, &dst_content);
    // Walk hunks ourselves so we can colorize each line by tag — the
    // built-in `unified_diff().to_string()` returns one flat string
    // with no ANSI escapes.
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

fn prompt_yes_no(question: &str) -> Result<bool> {
    use std::io::Write as _;
    eprint!("{question} [y/N]: ");
    std::io::stderr().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
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
            let src_str = engine.render(entry.src.as_str(), tera_ctx)?;
            let candidate = paths::resolve_mount_src(source, src_str.trim()).join(rel);
            // Honor `.yuiignore` even on manual absorb — if you've
            // ignored a path, you've explicitly opted out of yui's
            // managing it. One-shot stack walk along the candidate's
            // parents picks up nested `.yuiignore` files too.
            if paths::is_ignored_at(source, &candidate, candidate.is_dir())? {
                continue;
            }
            return Ok(Some(candidate));
        }
    }

    // 2. `.yuilink` Override markers — walk source, parse, render each
    //    `[[link]] dst`, see if target is the rendered dst (or nested
    //    inside a junction'd dir). `source_walker` skips `.yui/` and
    //    honours nested `.yuiignore` files automatically, so markers
    //    inside ignored subtrees never reach this loop.
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
        let spec = match marker::read_spec(&dir_utf8, marker_filename)? {
            Some(s) => s,
            None => continue,
        };
        let MarkerSpec::Explicit { links } = spec else {
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
            // File-level entry: dst points at a single file, so a match
            // resolves directly to `<marker-dir>/<src filename>`. Mirror
            // the existence check that apply / status do so a missing
            // sibling produces the same clear message regardless of
            // entry point — consistent with the `marker at … src=… not
            // found` shape users already see from those flows.
            if let Some(filename) = &link.src {
                let file_src = dir_utf8.join(filename);
                if !file_src.is_file() {
                    anyhow::bail!(
                        "marker at {dir_utf8}: [[link]] src={filename:?} \
                         not found"
                    );
                }
                if target == dst {
                    return Ok(Some(file_src));
                }
                continue;
            }
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

pub fn doctor(
    source: Option<Utf8PathBuf>,
    icons_override: Option<IconsMode>,
    no_color: bool,
) -> Result<()> {
    use owo_colors::OwoColorize as _;

    // Resolve source up-front so probes that depend on it can short-circuit
    // gracefully. A missing source is the single most common cause of yui
    // misbehaving, so we want to surface it loudly and skip the dependent
    // probes rather than blowing up.
    let resolved_source = resolve_source(source);

    // `YuiVars::detect` reads `yui.source` from the resolved source path
    // (so `{{ yui.source }}` renders correctly in config templates); when
    // no source is detected we fall back to `.` so identity probes can
    // still report os/arch/user/host.
    let yui = match &resolved_source {
        Ok(s) => YuiVars::detect(s),
        Err(_) => YuiVars::detect(Utf8Path::new(".")),
    };

    // Cache the loaded config — both the icons-override fallback and the
    // hooks-section probe need it. `cfg_res` keeps the original error
    // around so the `repo / config` probe can render a meaningful
    // message instead of just "not loaded".
    let cfg_res = match &resolved_source {
        Ok(s) => Some(config::load(s, &yui)),
        Err(_) => None,
    };
    let cfg = cfg_res.as_ref().and_then(|r| r.as_ref().ok());
    let icons_mode = icons_override
        .or_else(|| cfg.map(|c| c.ui.icons))
        .unwrap_or_default();
    let icons = Icons::for_mode(icons_mode);
    let color = !no_color && supports_color_stdout();

    let mut probes: Vec<Probe> = Vec::new();

    // ── identity ──────────────────────────────────────────────
    probes.push(Probe::group("identity"));
    probes.push(Probe::ok("os/arch", format!("{} / {}", yui.os, yui.arch)));
    probes.push(Probe::ok("user@host", format!("{}@{}", yui.user, yui.host)));

    // ── repository ────────────────────────────────────────────
    probes.push(Probe::group("repo"));
    let mut have_source = false;
    match &resolved_source {
        Ok(s) => {
            have_source = true;
            probes.push(Probe::ok("source", s.to_string()));
            match cfg_res.as_ref().expect("cfg_res set when source is Ok") {
                Ok(c) => {
                    probes.push(Probe::ok(
                        "config",
                        format!(
                            "{} mount{} · {} hook{} · {} render rule{}",
                            c.mount.entry.len(),
                            plural(c.mount.entry.len()),
                            c.hook.len(),
                            plural(c.hook.len()),
                            c.render.rule.len(),
                            plural(c.render.rule.len()),
                        ),
                    ));
                }
                Err(e) => probes.push(Probe::error("config", format!("{e}"))),
            }
            // git-clean check is informational here — the actual gate is
            // `[absorb] require_clean_git` on apply; warn so the user
            // knows auto-absorb will defer if they have uncommitted work.
            match crate::git::is_clean(s) {
                Ok(true) => probes.push(Probe::ok("git", "clean")),
                Ok(false) => probes.push(Probe::warn(
                    "git",
                    "uncommitted changes — `[absorb] require_clean_git` will defer auto-absorb",
                )),
                Err(_) => probes.push(Probe::warn(
                    "git",
                    "no git repo (auto-absorb still works; commit history won't track drift)",
                )),
            }
        }
        Err(e) => {
            probes.push(Probe::error("source", format!("not found — {e}")));
        }
    }

    // ── link / render mode ────────────────────────────────────
    probes.push(Probe::group("links"));
    if cfg!(windows) {
        probes.push(Probe::ok(
            "default mode",
            "files=hardlink, dirs=junction (no admin needed)",
        ));
    } else {
        probes.push(Probe::ok("default mode", "files=symlink, dirs=symlink"));
    }

    // ── hooks ─────────────────────────────────────────────────
    if have_source {
        if let (Ok(s), Some(c)) = (&resolved_source, cfg) {
            probes.push(Probe::group("hooks"));
            if c.hook.is_empty() {
                probes.push(Probe::ok("hooks", "(none configured)"));
            } else {
                let mut missing = 0usize;
                for h in &c.hook {
                    if !s.join(&h.script).is_file() {
                        missing += 1;
                        probes.push(Probe::error(
                            format!("hook[{}]", h.name),
                            format!("script not found at {}", h.script),
                        ));
                    }
                }
                if missing == 0 {
                    probes.push(Probe::ok(
                        "scripts",
                        format!(
                            "{} hook{} configured, all scripts present",
                            c.hook.len(),
                            plural(c.hook.len())
                        ),
                    ));
                }
            }
        }
    }

    // ── chezmoi cleanup hint ─────────────────────────────────
    if let Some(home) = paths::home_dir() {
        let chezmoi_src = home.join(".local/share/chezmoi");
        if chezmoi_src.is_dir() {
            probes.push(Probe::group("chezmoi"));
            probes.push(Probe::warn(
                "legacy source",
                format!(
                    "{chezmoi_src} still exists — yui doesn't use it, safe to archive once your migration has settled"
                ),
            ));
        }
    }

    // Render
    println!();
    if color {
        println!("  {}", "yui doctor".bold().underline());
    } else {
        println!("  yui doctor");
    }
    println!();
    for probe in &probes {
        probe.print(&icons, color);
    }

    let errors = probes.iter().filter(|p| p.is_error()).count();
    let warns = probes.iter().filter(|p| p.is_warn()).count();
    let oks = probes.iter().filter(|p| p.is_ok()).count();
    println!();
    let summary = format!("{oks} ok · {warns} warn · {errors} error");
    if color {
        if errors > 0 {
            println!("  {}", summary.red().bold());
        } else if warns > 0 {
            println!("  {}", summary.yellow());
        } else {
            println!("  {}", summary.green());
        }
    } else {
        println!("  {summary}");
    }

    if errors > 0 {
        anyhow::bail!("doctor: {errors} probe(s) failed");
    }
    Ok(())
}

#[derive(Debug)]
enum Probe {
    /// Section divider (just a heading, no severity).
    Group(&'static str),
    Ok {
        label: String,
        detail: String,
    },
    Warn {
        label: String,
        detail: String,
    },
    Error {
        label: String,
        detail: String,
    },
}

impl Probe {
    fn group(label: &'static str) -> Self {
        Self::Group(label)
    }
    fn ok(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Ok {
            label: label.into(),
            detail: detail.into(),
        }
    }
    fn warn(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Warn {
            label: label.into(),
            detail: detail.into(),
        }
    }
    fn error(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Error {
            label: label.into(),
            detail: detail.into(),
        }
    }
    fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }
    fn is_warn(&self) -> bool {
        matches!(self, Self::Warn { .. })
    }
    fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }
    fn print(&self, icons: &Icons, color: bool) {
        use owo_colors::OwoColorize as _;
        match self {
            Self::Group(name) => {
                println!();
                if color {
                    println!("  {}", name.cyan().bold());
                } else {
                    println!("  {name}");
                }
            }
            Self::Ok { label, detail } => {
                let icon = icons.ok;
                // Pad the raw label first; styling adds invisible ANSI
                // bytes that `format!("{:<14}")` would count as visible
                // width and silently break alignment between rows.
                let padded = format!("{label:<14}");
                if color {
                    println!(
                        "    {}  {}  {}",
                        icon.green(),
                        padded.bold(),
                        detail.dimmed()
                    );
                } else {
                    println!("    {icon}  {padded}  {detail}");
                }
            }
            Self::Warn { label, detail } => {
                let icon = icons.warn;
                let padded = format!("{label:<14}");
                if color {
                    println!(
                        "    {}  {}  {}",
                        icon.yellow(),
                        padded.bold().yellow(),
                        detail
                    );
                } else {
                    println!("    {icon}  {padded}  {detail}");
                }
            }
            Self::Error { label, detail } => {
                let icon = icons.error;
                let padded = format!("{label:<14}");
                if color {
                    println!(
                        "    {}  {}  {}",
                        icon.red().bold(),
                        padded.bold().red(),
                        detail.red()
                    );
                } else {
                    println!("    {icon}  {padded}  {detail}");
                }
            }
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `yui gc-backup [--older-than DUR] [--dry-run]` — prune snapshots
/// under `$DOTFILES/.yui/backup/`.
///
/// With no `--older-than` we run a non-destructive *survey*: walk the
/// backup tree, list every entry whose name carries yui's
/// `_<YYYYMMDD_HHMMSSfff>[.<ext>]` suffix, and print AGE / SIZE / PATH
/// sorted oldest-first plus a hint to pass `--older-than DUR` to
/// actually delete. With `--older-than DUR` (e.g. `30d`, `2w`, `12h`,
/// `6m`, `1y`) we delete every entry strictly older than the cutoff.
/// `--dry-run` previews the same set without writing.
///
/// Two design points worth flagging:
/// 1. *Suffix, not mtime.* `std::fs::copy` preserves source mtime on
///    most platforms, so a backup of an old dotfile would look
///    "old" by mtime even when freshly created. The suffix is the
///    source of truth for "when did yui take this snapshot?".
/// 2. *Defensive parse.* Anything in `.yui/backup/` whose name
///    doesn't match the suffix shape is left alone — if you dropped
///    a file there by hand, gc-backup isn't going to delete it.
pub fn gc_backup(
    source: Option<Utf8PathBuf>,
    older_than: Option<String>,
    dry_run: bool,
    icons_override: Option<IconsMode>,
    no_color: bool,
) -> Result<()> {
    let source = resolve_source(source)?;
    let yui = YuiVars::detect(&source);
    let config = config::load(&source, &yui)?;
    let backup_root = source.join(&config.backup.dir);
    let icons_mode = icons_override.unwrap_or(config.ui.icons);
    let icons = Icons::for_mode(icons_mode);
    let color = !no_color && supports_color_stdout();

    if !backup_root.is_dir() {
        println!("  no backup tree at {backup_root}");
        return Ok(());
    }

    let mut entries = walk_gc_backups(&backup_root)?;
    if entries.is_empty() {
        println!("  no yui-stamped backups under {backup_root}");
        return Ok(());
    }
    // Oldest first — that's the natural "what should I prune?" order.
    entries.sort_by_key(|e| e.ts);
    let now = jiff::Zoned::now();

    match older_than {
        None => {
            let refs: Vec<&BackupEntry> = entries.iter().collect();
            print_gc_table(&refs, &backup_root, &now, icons, color);
            println!();
            println!(
                "  {} entries · {} total — pass --older-than DUR (e.g. 30d) to delete",
                entries.len(),
                format_bytes(entries.iter().map(|e| e.size_bytes).sum())
            );
            Ok(())
        }
        Some(dur_str) => {
            let span = parse_human_duration(&dur_str)?;
            let cutoff = now
                .checked_sub(span)
                .map_err(|e| anyhow::anyhow!("invalid duration {dur_str:?}: {e}"))?;
            let cutoff_dt = cutoff.datetime();

            let total_before: u64 = entries.iter().map(|e| e.size_bytes).sum();
            let to_delete: Vec<&BackupEntry> =
                entries.iter().filter(|e| e.ts < cutoff_dt).collect();

            if to_delete.is_empty() {
                println!(
                    "  no backups older than {dur_str} (oldest: {})",
                    format_age(entries[0].ts, &now)
                );
                return Ok(());
            }

            print_gc_table(&to_delete, &backup_root, &now, icons, color);
            println!();
            let total_freed: u64 = to_delete.iter().map(|e| e.size_bytes).sum();

            if dry_run {
                println!(
                    "  [dry-run] would remove {} of {} entries · would free {} of {}",
                    to_delete.len(),
                    entries.len(),
                    format_bytes(total_freed),
                    format_bytes(total_before),
                );
                return Ok(());
            }

            for entry in &to_delete {
                match entry.kind {
                    BackupKind::File => std::fs::remove_file(&entry.path)?,
                    BackupKind::Dir => std::fs::remove_dir_all(&entry.path)?,
                }
                if let Some(parent) = entry.path.parent() {
                    cleanup_empty_parents(parent, &backup_root);
                }
            }
            println!(
                "  removed {} of {} entries · freed {} (was {}, now {})",
                to_delete.len(),
                entries.len(),
                format_bytes(total_freed),
                format_bytes(total_before),
                format_bytes(total_before - total_freed),
            );
            Ok(())
        }
    }
}

#[derive(Debug)]
struct BackupEntry {
    path: Utf8PathBuf,
    ts: jiff::civil::DateTime,
    kind: BackupKind,
    size_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackupKind {
    File,
    Dir,
}

/// Recursive walk that recognises directory backups as one unit
/// (so we don't descend into `<dirname>_<ts>/` and surface its
/// individual files — the whole subtree is one snapshot). Files
/// without a yui suffix are silently skipped.
fn walk_gc_backups(root: &Utf8Path) -> Result<Vec<BackupEntry>> {
    let mut out = Vec::new();
    walk_gc_backups_rec(root, &mut out)?;
    Ok(out)
}

fn walk_gc_backups_rec(dir: &Utf8Path, out: &mut Vec<BackupEntry>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        let path = dir.join(name);
        let ft = entry.file_type()?;
        if ft.is_dir() {
            if let Some(ts) = parse_backup_suffix(name) {
                let size = dir_size(&path)?;
                out.push(BackupEntry {
                    path,
                    ts,
                    kind: BackupKind::Dir,
                    size_bytes: size,
                });
            } else {
                walk_gc_backups_rec(&path, out)?;
            }
        } else if ft.is_file() {
            // Nested ifs (not let-chains) so the crate's MSRV
            // (rust-version = "1.85") stays buildable.
            if let Some(ts) = parse_backup_suffix(name) {
                let size = entry.metadata()?.len();
                out.push(BackupEntry {
                    path,
                    ts,
                    kind: BackupKind::File,
                    size_bytes: size,
                });
            }
        }
    }
    Ok(())
}

fn dir_size(dir: &Utf8Path) -> Result<u64> {
    let mut total: u64 = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            let p = match Utf8PathBuf::from_path_buf(entry.path()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            total = total.saturating_add(dir_size(&p)?);
        } else if ft.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

/// Walk up from `start` toward `root`, removing any directory that
/// has become empty as a result of a deletion. Stops at the first
/// non-empty parent and never touches `root` itself.
fn cleanup_empty_parents(start: &Utf8Path, root: &Utf8Path) {
    let mut cur = start.to_path_buf();
    loop {
        if cur == *root {
            return;
        }
        // remove_dir succeeds only if the directory is empty.
        if std::fs::remove_dir(&cur).is_err() {
            return;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return,
        }
    }
}

/// Parse a yui backup name. Two shapes:
///   - `<stem>_<YYYYMMDD_HHMMSSfff>`            (dirs / dotfiles / no-ext)
///   - `<stem>_<YYYYMMDD_HHMMSSfff>.<ext>`      (files with extension)
///
/// Returns the timestamp on success, `None` for anything else.
fn parse_backup_suffix(name: &str) -> Option<jiff::civil::DateTime> {
    if let Some(ts) = parse_ts_at_end(name) {
        return Some(ts);
    }
    // Nested ifs (not let-chains) so the crate's MSRV
    // (rust-version = "1.85") stays buildable.
    if let Some((before, _ext)) = name.rsplit_once('.') {
        if let Some(ts) = parse_ts_at_end(before) {
            return Some(ts);
        }
    }
    None
}

fn parse_ts_at_end(s: &str) -> Option<jiff::civil::DateTime> {
    // Need at least 1 stem char + `_` + 18-char timestamp.
    if s.len() < 20 {
        return None;
    }
    let split_at = s.len() - 19;
    if s.as_bytes()[split_at] != b'_' {
        return None;
    }
    parse_ts(&s[split_at + 1..])
}

/// Parse exactly `YYYYMMDD_HHMMSSfff`.
fn parse_ts(s: &str) -> Option<jiff::civil::DateTime> {
    if s.len() != 18 || s.as_bytes()[8] != b'_' {
        return None;
    }
    for (i, &b) in s.as_bytes().iter().enumerate() {
        if i == 8 {
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
    }
    let year: i16 = s[0..4].parse().ok()?;
    let month: i8 = s[4..6].parse().ok()?;
    let day: i8 = s[6..8].parse().ok()?;
    let hour: i8 = s[9..11].parse().ok()?;
    let minute: i8 = s[11..13].parse().ok()?;
    let second: i8 = s[13..15].parse().ok()?;
    let ms: i32 = s[15..18].parse().ok()?;
    jiff::civil::DateTime::new(year, month, day, hour, minute, second, ms * 1_000_000).ok()
}

/// Parse a duration string in the shorthand `30d`, `2w`, `12h`,
/// `6mo` (months), `1y`, `5m` (minutes). Whitespace around the
/// number is tolerated; the unit is case-insensitive.
///
/// `m` means **minutes**, `mo` means **months** — bare `m` matches
/// what `format_age` prints in the survey table, so a backup
/// shown as "5m" is pruneable as `--older-than 5m`. Months take
/// the explicit `mo` form. (Caught in PR #51 review.)
fn parse_human_duration(s: &str) -> Result<jiff::Span> {
    let s = s.trim();
    let split = s
        .bytes()
        .position(|b| b.is_ascii_alphabetic())
        .ok_or_else(|| anyhow::anyhow!("invalid duration {s:?}: missing unit (e.g. 30d, 2w)"))?;
    let n: i64 = s[..split]
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration {s:?}: bad leading number"))?;
    if n < 0 {
        anyhow::bail!("invalid duration {s:?}: negative durations don't make sense");
    }
    let unit = s[split..].to_ascii_lowercase();
    let span = match unit.as_str() {
        "y" | "yr" | "year" | "years" => jiff::Span::new().years(n),
        "mo" | "month" | "months" => jiff::Span::new().months(n),
        "w" | "wk" | "week" | "weeks" => jiff::Span::new().weeks(n),
        "d" | "day" | "days" => jiff::Span::new().days(n),
        "h" | "hr" | "hour" | "hours" => jiff::Span::new().hours(n),
        "m" | "min" | "minute" | "minutes" => jiff::Span::new().minutes(n),
        other => {
            anyhow::bail!(
                "invalid duration {s:?}: unknown unit {other:?} \
                 (use y / mo / w / d / h / m)"
            )
        }
    };
    Ok(span)
}

fn format_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if n >= GIB {
        format!("{:.1} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

fn format_age(ts: jiff::civil::DateTime, now: &jiff::Zoned) -> String {
    let Ok(ts_zoned) = ts.to_zoned(now.time_zone().clone()) else {
        return "?".into();
    };
    let secs = match (now - &ts_zoned).total(jiff::Unit::Second) {
        Ok(s) => s as i64,
        Err(_) => return "?".into(),
    };
    if secs < 0 {
        return "future".into();
    }
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 86_400 * 30 {
        format!("{}d", secs / 86_400)
    } else if secs < 86_400 * 365 {
        format!("{}mo", secs / (86_400 * 30))
    } else {
        format!("{}y", secs / (86_400 * 365))
    }
}

/// Render a borrowed slice of `BackupEntry`s as an AGE / SIZE / PATH
/// table. Trailing `/` on the path marks dir backups (filesystem
/// convention) so the kind is visible without a dedicated column.
/// `_icons` is currently unused but kept on the signature so future
/// table changes can adopt new glyphs without rippling through every
/// caller.
fn print_gc_table(
    entries: &[&BackupEntry],
    backup_root: &Utf8Path,
    now: &jiff::Zoned,
    _icons: Icons,
    color: bool,
) {
    use owo_colors::OwoColorize as _;

    let rows: Vec<(String, String, String)> = entries
        .iter()
        .map(|e| {
            let rel = e
                .path
                .strip_prefix(backup_root)
                .map(Utf8PathBuf::from)
                .unwrap_or_else(|_| e.path.clone());
            let path_disp = match e.kind {
                BackupKind::Dir => format!("{rel}/"),
                BackupKind::File => rel.to_string(),
            };
            (format_age(e.ts, now), format_bytes(e.size_bytes), path_disp)
        })
        .collect();

    let age_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(3);
    let size_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(4);

    if color {
        println!(
            "  {:<age_w$}  {:>size_w$}  {}",
            "AGE".dimmed(),
            "SIZE".dimmed(),
            "PATH".dimmed(),
        );
    } else {
        println!("  {:<age_w$}  {:>size_w$}  PATH", "AGE", "SIZE");
    }
    for (age, size, path) in &rows {
        if color {
            println!(
                "  {:<age_w$}  {:>size_w$}  {}",
                age.yellow(),
                size,
                path.cyan(),
            );
        } else {
            println!("  {:<age_w$}  {:>size_w$}  {}", age, size, path);
        }
    }
}

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

fn link_file_with_backup(src: &Utf8Path, dst: &Utf8Path, ctx: &ApplyCtx<'_>) -> Result<()> {
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
fn absorb_target_into_source(src: &Utf8Path, dst: &Utf8Path, ctx: &ApplyCtx<'_>) -> Result<()> {
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
fn overwrite_source_into_target(src: &Utf8Path, dst: &Utf8Path, ctx: &ApplyCtx<'_>) -> Result<()> {
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
fn prompt_anomaly(
    ctx: &ApplyCtx<'_>,
    src: &Utf8Path,
    dst: &Utf8Path,
    reason: &str,
) -> Result<AnomalyChoice> {
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
        let choice = match trimmed {
            "" | "s" => AnomalyChoice::Skip,
            "a" => AnomalyChoice::Absorb,
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
fn absorb_target_dir_into_source(src: &Utf8Path, dst: &Utf8Path, ctx: &ApplyCtx<'_>) -> Result<()> {
    info!("absorb dir: {dst} → {src}");
    backup_existing(src, ctx.backup_root, /* is_dir */ true)?;
    merge_dir_target_into_source(dst, src, ctx)?;
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

const SKELETON_GITIGNORE: &str = r#"# yui per-machine state and backups (regenerable, do not commit).
# .yui/bin/ is intentionally tracked — it holds your hook scripts.
/.yui/state.json
/.yui/state.json.tmp
/.yui/backup/

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
    fn apply_marker_inactive_link_falls_through_to_default() {
        // v0.6+ semantics: a marker that has only inactive links no
        // longer suppresses the parent mount's natural placement. The
        // walker keeps descending so per-file defaults still apply.
        // (Use `.yuiignore` to actually exclude a subtree.)
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

        // Inactive marker target untouched.
        assert!(!target_inactive.join("nvim").exists());
        // Parent mount's natural placement IS produced — the marker had
        // no active dir-level link to claim coverage with.
        assert!(parent_target.join(".config/nvim/init.lua").exists());
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

    /// `init` is now in charge of the `.yui/` state / backup ignore
    /// lines, even on a re-run against an existing repo. Pre-fix it
    /// silently left a half-populated `.gitignore` alone if the user
    /// didn't have the entries in place; now it appends the missing
    /// ones idempotently.
    #[test]
    fn init_appends_missing_gitignore_entries_into_existing_file() {
        let tmp = TempDir::new().unwrap();
        let dir = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(&dir).unwrap();
        // Existing .gitignore that DOESN'T yet have any yui entries.
        let user_gitignore = "# user entries\n*.swp\nnode_modules/\n";
        std::fs::write(dir.join(".gitignore"), user_gitignore).unwrap();

        init(Some(dir.clone()), false).unwrap();

        let body = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        // The user's existing lines survive untouched.
        assert!(body.contains("*.swp"));
        assert!(body.contains("node_modules/"));
        // Each yui-required line was appended.
        assert!(body.contains("/.yui/state.json"));
        assert!(body.contains("/.yui/backup/"));
        assert!(body.contains("config.local.toml"));
        // Re-running init on the already-fixed-up file is a no-op.
        let before_rerun = body.clone();
        // `init` would normally bail on an existing config; remove it so
        // the second call doesn't trip that guard.
        std::fs::remove_file(dir.join("config.toml")).unwrap();
        init(Some(dir.clone()), false).unwrap();
        let after_rerun = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(
            before_rerun, after_rerun,
            "init must be idempotent when the gitignore already has every yui entry"
        );
    }

    /// `init --git-hooks` against an *existing* repo (config.toml
    /// already there) skips the scaffold and just installs the hooks.
    /// Pre-fix this combo bailed with "config.toml already exists",
    /// which forced users with a populated dotfiles repo to delete
    /// their config before they could opt into the render-drift hooks.
    #[test]
    fn init_with_git_hooks_installs_into_existing_repo() {
        let tmp = TempDir::new().unwrap();
        let dir = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(&dir).unwrap();
        let st = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.as_std_path())
            .status()
            .expect("git init");
        if !st.success() {
            return;
        }
        // Pre-existing user config — init should NOT overwrite it.
        let user_config = "# user already wrote this\n";
        std::fs::write(dir.join("config.toml"), user_config).unwrap();

        // hooks-only invocation: succeeds, leaves config alone.
        init(Some(dir.clone()), /* git_hooks */ true).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).unwrap(),
            user_config
        );
        assert!(dir.join(".git/hooks/pre-commit").is_file());
        assert!(dir.join(".git/hooks/pre-push").is_file());
    }

    /// `init --git-hooks` writes pre-commit / pre-push that run the
    /// render-drift check against `.git/hooks/`. We need a real git
    /// repo for `git rev-parse --git-path hooks` to point at, so
    /// prepare one before calling init.
    #[test]
    fn init_with_git_hooks_writes_pre_commit_and_pre_push() {
        let tmp = TempDir::new().unwrap();
        let dir = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(&dir).unwrap();
        // Bootstrap a git repo at `dir`.
        let st = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.as_std_path())
            .status()
            .expect("git init");
        if !st.success() {
            // Skip if git isn't on PATH on this CI runner.
            eprintln!("skipping: git not available");
            return;
        }
        init(Some(dir.clone()), /* git_hooks */ true).unwrap();

        let pre_commit = dir.join(".git/hooks/pre-commit");
        let pre_push = dir.join(".git/hooks/pre-push");
        assert!(pre_commit.is_file(), "pre-commit hook should be written");
        assert!(pre_push.is_file(), "pre-push hook should be written");

        let body = std::fs::read_to_string(&pre_commit).unwrap();
        assert!(
            body.contains("yui render --check"),
            "pre-commit hook should call `yui render --check`, got: {body}"
        );
    }

    /// `init --git-hooks` against a non-git directory must fail with a
    /// clear message instead of silently doing nothing — the user
    /// asked for hooks and we couldn't deliver.
    #[test]
    fn init_with_git_hooks_errors_outside_a_git_repo() {
        let tmp = TempDir::new().unwrap();
        let dir = utf8(tmp.path().join("not-a-repo"));
        std::fs::create_dir_all(&dir).unwrap();
        let err = init(Some(dir), /* git_hooks */ true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("git repo") || msg.contains("git rev-parse"),
            "expected error to mention the git issue, got: {msg}"
        );
    }

    /// Pre-existing hooks are not silently overwritten — yui leaves
    /// the user's prior file alone (warns) and writes the missing one.
    #[test]
    fn init_with_git_hooks_does_not_clobber_existing_hooks() {
        let tmp = TempDir::new().unwrap();
        let dir = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(&dir).unwrap();
        let st = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.as_std_path())
            .status()
            .expect("git init");
        if !st.success() {
            return;
        }
        let hooks = dir.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-commit"), "#! /bin/sh\nexit 0\n").unwrap();

        init(Some(dir.clone()), true).unwrap();

        // Existing pre-commit untouched, pre-push freshly written.
        let pc = std::fs::read_to_string(hooks.join("pre-commit")).unwrap();
        assert!(
            !pc.contains("yui render --check"),
            "existing pre-commit must not be overwritten"
        );
        let pp = std::fs::read_to_string(hooks.join("pre-push")).unwrap();
        assert!(
            pp.contains("yui render --check"),
            "missing pre-push should be written: {pp}"
        );
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

    /// Regression for the Windows-error-145 bug: a `home/.config/.yuilink`
    /// (PassThrough) marker pointing at a non-empty regular `~/.config`
    /// directory (the typical chezmoi-migrated state, where every file
    /// inside is an individual hardlink) used to fail the absorb with
    /// `Directory not empty` because `link::unlink` refuses to recurse.
    /// After backup we now `remove_dir_all` as a fallback.
    ///
    /// v0.7+: also exercises the target-wins merge — target's
    /// `config.toml` overwrites source's, target's `state.json` lands
    /// in source (target was the source of truth), and source-only
    /// scaffolding (`.yuilink`) survives the absorb.
    #[test]
    fn apply_absorbs_non_empty_target_dir_target_wins() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home/.config/app")).unwrap();
        std::fs::create_dir_all(target.join(".config/app")).unwrap();
        // Marker that says "junction this dir at the parent mount's dst"
        // — same shape as a typical home/.config/.yuilink.
        std::fs::write(source.join("home/.config/.yuilink"), "").unwrap();
        std::fs::write(source.join("home/.config/app/config.toml"), "src side").unwrap();
        // Source-only scaffolding that the absorb must preserve.
        std::fs::write(source.join("home/.config/app/source-only.toml"), "src").unwrap();
        // Pre-existing non-empty regular dir at the target — chezmoi /
        // any per-file dotfiles flow leaves things in this shape.
        std::fs::write(target.join(".config/app/config.toml"), "target side").unwrap();
        std::fs::write(target.join(".config/app/state.json"), "{}").unwrap();

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

        // Used to bail with `unlink: ... Directory not empty` here.
        apply(Some(source.clone()), false).unwrap();

        // Target wins on the conflicting file.
        assert_eq!(
            std::fs::read_to_string(target.join(".config/app/config.toml")).unwrap(),
            "target side"
        );
        // Target-only file is now reachable via the junction.
        assert_eq!(
            std::fs::read_to_string(target.join(".config/app/state.json")).unwrap(),
            "{}"
        );
        // Source's pre-merge state was backed up before being overwritten,
        // so the original "src side" / `.yuilink` survive in `.yui/backup/`.
        let backup_root = source.join(".yui/backup");
        let mut backup_files: Vec<String> = Vec::new();
        for entry in walkdir(&backup_root) {
            if let Some(n) = entry.file_name() {
                backup_files.push(n.to_string());
            }
        }
        assert!(
            backup_files.iter().any(|f| f == "config.toml"),
            "expected source's config.toml to land in the backup tree, got {backup_files:?}"
        );
        // Source-only scaffolding survives the merge.
        assert!(
            source.join("home/.config/app/source-only.toml").exists(),
            "source-only file should survive a target-wins merge"
        );
        // Source picked up target-only state.json via the merge.
        assert!(
            source.join("home/.config/app/state.json").exists(),
            "target-only state.json should be merged into source"
        );
    }

    /// v0.7+: `home/.config/.yuilink` is the user's explicit
    /// "this whole subtree is target-as-truth" declaration. A
    /// dir-level NeedsConfirm at the marker root is therefore not a
    /// real anomaly — the marker is consent. Default `[absorb]` (ask
    /// + require_clean_git) should still absorb, no prompt.
    #[test]
    fn marker_dir_absorbs_with_default_ask_policy() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home/.config")).unwrap();
        std::fs::create_dir_all(target.join(".config/gh")).unwrap();
        // Marker — user opts the whole .config dir into target-as-truth.
        std::fs::write(source.join("home/.config/.yuilink"), "").unwrap();
        // gh exists only on the target side (no entry in source).
        std::fs::write(target.join(".config/gh/hosts.yml"), "oauth_token: x\n").unwrap();

        // Default [absorb] (no override) — `on_anomaly = "ask"`,
        // `auto = true`, `require_clean_git = true`. Pre-v0.7 this
        // would have been routed through the ask prompt at dir level.
        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"
"#,
            toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        // Even with default `ask`, the marker-rooted absorb proceeds.
        // Test would hang on a stdin prompt if dir-level still treated
        // this as an anomaly.
        apply(Some(source.clone()), false).unwrap();

        // Target-only file is now reachable through the junction and
        // recorded in source.
        assert!(target.join(".config/gh/hosts.yml").exists());
        assert!(source.join("home/.config/gh/hosts.yml").exists());
    }

    /// File↔dir collisions during merge. Honor target-wins: if source
    /// has a regular file at a path where target has a dir, the file
    /// gets removed and the dir is created. Symmetrical for the
    /// inverse case. Without the conflict-clearing the merge would
    /// fail with `not a directory` / `path exists` deep in the recursion.
    #[test]
    fn merge_handles_file_vs_dir_collisions_target_wins() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home/.config/foo")).unwrap();
        std::fs::create_dir_all(target.join(".config")).unwrap();
        std::fs::write(source.join("home/.config/.yuilink"), "").unwrap();

        // Conflict A: source has `foo` as dir, target has `foo` as file.
        std::fs::write(source.join("home/.config/foo/leaf.txt"), "src").unwrap();
        std::fs::write(target.join(".config/foo"), "target file body").unwrap();
        // Conflict B: source has `bar` as file, target has `bar` as dir.
        std::fs::write(source.join("home/.config/bar"), "src file body").unwrap();
        std::fs::create_dir_all(target.join(".config/bar")).unwrap();
        std::fs::write(target.join(".config/bar/inside.txt"), "target nested").unwrap();

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
        apply(Some(source.clone()), false).unwrap();

        // After absorb the target's view (which equals source via
        // junction) carries target's shapes:
        // `foo` is a regular file
        let foo_meta = std::fs::symlink_metadata(target.join(".config/foo")).unwrap();
        assert!(foo_meta.file_type().is_file(), "foo should be a file");
        assert_eq!(
            std::fs::read_to_string(target.join(".config/foo")).unwrap(),
            "target file body"
        );
        // `bar` is a directory with the nested file
        let bar_meta = std::fs::symlink_metadata(target.join(".config/bar")).unwrap();
        assert!(bar_meta.file_type().is_dir(), "bar should be a dir");
        assert_eq!(
            std::fs::read_to_string(target.join(".config/bar/inside.txt")).unwrap(),
            "target nested"
        );
    }

    /// Per-file conflict in dir merge — target newer + content
    /// differs → AutoAbsorb. Target wins automatically without
    /// touching `[absorb] on_anomaly`.
    #[test]
    fn merge_per_file_target_newer_auto_absorbs() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home/.config")).unwrap();
        std::fs::create_dir_all(target.join(".config")).unwrap();
        std::fs::write(source.join("home/.config/.yuilink"), "").unwrap();

        // Source has the older copy, target has the newer edit.
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        write_with_mtime(&source.join("home/.config/app.toml"), "old src", past);
        std::fs::write(target.join(".config/app.toml"), "user's live edit").unwrap();

        // Default `ask` policy — should NOT prompt because the
        // classifier returns AutoAbsorb (target newer + diff), which
        // bypasses `on_anomaly` entirely.
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

        // Target wins.
        assert_eq!(
            std::fs::read_to_string(target.join(".config/app.toml")).unwrap(),
            "user's live edit"
        );
    }

    /// Per-file conflict — source newer + content differs +
    /// `on_anomaly = "skip"` → keep source's version. After the outer
    /// junction, target ends up with source's content (so target's
    /// file is effectively dropped, matching the file-level `skip`
    /// semantic).
    #[test]
    fn merge_per_file_source_newer_skip_keeps_source() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home/.config")).unwrap();
        std::fs::create_dir_all(target.join(".config")).unwrap();
        std::fs::write(source.join("home/.config/.yuilink"), "").unwrap();

        // Target has the older copy, source has the newer edit.
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        write_with_mtime(&target.join(".config/app.toml"), "old target", past);
        std::fs::write(source.join("home/.config/app.toml"), "fresh source").unwrap();

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
        apply(Some(source.clone()), false).unwrap();

        // Source kept — target now reads source's version through the
        // junction (so target's old text is dropped).
        assert_eq!(
            std::fs::read_to_string(target.join(".config/app.toml")).unwrap(),
            "fresh source"
        );
    }

    /// Per-file conflict — source newer + content differs +
    /// `on_anomaly = "force"` → target wins anyway.
    #[test]
    fn merge_per_file_source_newer_force_overwrites_source() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home/.config")).unwrap();
        std::fs::create_dir_all(target.join(".config")).unwrap();
        std::fs::write(source.join("home/.config/.yuilink"), "").unwrap();

        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        write_with_mtime(&target.join(".config/app.toml"), "old target", past);
        std::fs::write(source.join("home/.config/app.toml"), "fresh source").unwrap();

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
        apply(Some(source.clone()), false).unwrap();

        // Target overrides source despite being mtime-older.
        assert_eq!(
            std::fs::read_to_string(target.join(".config/app.toml")).unwrap(),
            "old target"
        );
    }

    /// Per-file conflict — bytes match → no-op. The merge classifies
    /// this as RelinkOnly and skips the copy entirely (saves a lot of
    /// I/O when migrating big chezmoi repos where source and target
    /// have already shared inodes).
    #[test]
    fn merge_per_file_identical_content_is_noop() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home/.config")).unwrap();
        std::fs::create_dir_all(target.join(".config")).unwrap();
        std::fs::write(source.join("home/.config/.yuilink"), "").unwrap();
        std::fs::write(source.join("home/.config/app.toml"), "same").unwrap();
        std::fs::write(target.join(".config/app.toml"), "same").unwrap();

        // Default policy — bytes match, classifier returns RelinkOnly,
        // merge skips the copy. Apply must succeed without prompting.
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

        assert_eq!(
            std::fs::read_to_string(target.join(".config/app.toml")).unwrap(),
            "same"
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

        // Run absorb directly on the target — `--yes` skips the
        // interactive prompt the manual flow normally requires.
        absorb(
            Some(source.clone()),
            target.join(".bashrc"),
            /* dry_run */ false,
            /* yes */ true,
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
        let err = absorb(Some(source), stranger, false, /* yes */ true).unwrap_err();
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

    /// Issue #47: a `.yuiignore` placed in a nested subdirectory must
    /// scope its rules to that subtree, just like `.gitignore`.
    /// `home/inner/.yuiignore` excluding `secret*` should drop
    /// `home/inner/secret.txt` but leave `home/secret.txt` alone.
    #[test]
    fn nested_yuiignore_only_affects_its_subtree() {
        let tmp = TempDir::new().unwrap();
        let (source, target) = setup_minimal_dotfiles(&tmp);
        std::fs::create_dir_all(source.join("home/inner")).unwrap();
        std::fs::write(source.join("home/secret.txt"), "outer keep").unwrap();
        std::fs::write(source.join("home/inner/secret.txt"), "inner drop").unwrap();
        std::fs::write(source.join("home/inner/keep.txt"), "inner keep").unwrap();
        // Nested ignore — affects only `home/inner/`.
        std::fs::write(source.join("home/inner/.yuiignore"), "secret*\n").unwrap();
        apply(Some(source.clone()), false).unwrap();
        assert!(
            target.join("secret.txt").exists(),
            "outer secret.txt is outside the nested .yuiignore scope"
        );
        assert!(target.join("inner/keep.txt").exists());
        assert!(
            !target.join("inner/secret.txt").exists(),
            "inner secret.txt should be excluded by the nested .yuiignore"
        );
    }

    /// A nested `.yuiignore` can re-include (via `!negation`) a file
    /// the root ignore had excluded — gitignore's last-rule-wins
    /// semantics, scoped per-subtree.
    #[test]
    fn nested_yuiignore_negation_overrides_root_rule() {
        let tmp = TempDir::new().unwrap();
        let (source, target) = setup_minimal_dotfiles(&tmp);
        std::fs::create_dir_all(source.join("home/keepers")).unwrap();
        std::fs::write(source.join("home/drop.lock"), "outer drop").unwrap();
        std::fs::write(source.join("home/keepers/wanted.lock"), "inner keep").unwrap();
        std::fs::write(source.join(".yuiignore"), "*.lock\n").unwrap();
        // Re-include `*.lock` only inside keepers/.
        std::fs::write(source.join("home/keepers/.yuiignore"), "!*.lock\n").unwrap();
        apply(Some(source.clone()), false).unwrap();
        assert!(
            !target.join("drop.lock").exists(),
            "root rule still drops outer .lock file"
        );
        assert!(
            target.join("keepers/wanted.lock").exists(),
            "nested negation re-includes .lock under keepers/"
        );
    }

    /// `yui status` walk uses the same nested-`.yuiignore` semantics:
    /// a nested ignore scoped to one subtree must NOT make a sibling
    /// subtree's identical filename look ignored.
    #[test]
    fn nested_yuiignore_status_walk_scoped() {
        let tmp = TempDir::new().unwrap();
        let (source, _target) = setup_minimal_dotfiles(&tmp);
        std::fs::create_dir_all(source.join("home/a")).unwrap();
        std::fs::create_dir_all(source.join("home/b")).unwrap();
        std::fs::write(source.join("home/a/foo.txt"), "a-foo").unwrap();
        std::fs::write(source.join("home/b/foo.txt"), "b-foo").unwrap();
        // Only `home/a/` ignores foo.txt.
        std::fs::write(source.join("home/a/.yuiignore"), "foo.txt\n").unwrap();
        apply(Some(source.clone()), false).unwrap();
        // status should not error; walk completes despite the nested rule.
        let res = status(Some(source), None, /* no_color */ true);
        assert!(res.is_ok() || matches!(&res, Err(e) if format!("{e}").contains("diverged")));
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

    // -----------------------------------------------------------------
    // secrets (age) end-to-end
    // -----------------------------------------------------------------

    /// `yui apply` decrypts every `*.age` to its sibling and the
    /// sibling lands in target as a regular file. The plaintext is
    /// also added to the managed `.gitignore` section so it doesn't
    /// get committed.
    #[test]
    fn apply_decrypts_age_files_to_sibling_and_links() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home/.ssh")).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        // 1. Generate a keypair, write identity file inside the test
        //    sandbox so we don't touch the user's real `~/.config/yui/`.
        let identity_path = utf8(tmp.path().join("age.txt"));
        let (secret, public) = secret::generate_x25519_keypair();
        std::fs::write(&identity_path, format!("{secret}\n")).unwrap();

        // 2. Encrypt a fake private key into source as `.age`.
        let recipient = secret::parse_x25519_recipient(&public).unwrap();
        let cipher = secret::encrypt_x25519(b"-- super secret key --\n", &[recipient]).unwrap();
        std::fs::write(source.join("home/.ssh/id_ed25519.age"), &cipher).unwrap();

        // 3. config.toml: mount + secrets pointing at the test identity.
        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"

[secrets]
identity = "{}"
recipients = ["{}"]
"#,
            toml_path(&target),
            toml_path(&identity_path),
            public
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        apply(Some(source.clone()), false).unwrap();

        // Plaintext sibling appeared.
        assert!(source.join("home/.ssh/id_ed25519").exists());
        // Target got the linked file with decrypted content.
        let target_bytes = std::fs::read(target.join(".ssh/id_ed25519")).unwrap();
        assert_eq!(target_bytes, b"-- super secret key --\n");
        // Plaintext path is in the managed .gitignore section.
        let gi = std::fs::read_to_string(source.join(".gitignore")).unwrap();
        assert!(
            gi.contains("home/.ssh/id_ed25519"),
            ".gitignore should list the decrypted plaintext sibling: {gi}"
        );
        // The .age ciphertext is the canonical, NOT in the managed list.
        // (It's expected to be committed normally.)
    }

    /// `yui apply` bails when the on-disk plaintext sibling has
    /// drifted from the canonical `.age`. Mirrors render-drift
    /// semantics: the user must run `yui secret encrypt` to roll
    /// the change back into ciphertext before re-running apply.
    #[test]
    fn apply_bails_on_secret_drift() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        let identity_path = utf8(tmp.path().join("age.txt"));
        let (secret_key, public) = secret::generate_x25519_keypair();
        std::fs::write(&identity_path, format!("{secret_key}\n")).unwrap();

        let recipient = secret::parse_x25519_recipient(&public).unwrap();
        let cipher = secret::encrypt_x25519(b"v1 content\n", &[recipient]).unwrap();
        std::fs::write(source.join("home/secret.age"), &cipher).unwrap();
        // Drifted sibling: plaintext exists but doesn't match the .age content.
        std::fs::write(source.join("home/secret"), "edited locally\n").unwrap();

        let cfg = format!(
            r#"
[[mount.entry]]
src = "home"
dst = "{}"

[secrets]
identity = "{}"
recipients = ["{}"]
"#,
            toml_path(&target),
            toml_path(&identity_path),
            public
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        let err = apply(Some(source.clone()), false).unwrap_err();
        assert!(
            format!("{err:#}").contains("secret drift"),
            "expected secret drift error, got: {err:#}"
        );
    }

    // -- append_recipient_to_config (PR #57 review: toml_edit) --

    #[test]
    fn append_recipient_creates_secrets_table_when_missing() {
        let result =
            append_recipient_to_config("", "host alice", "age1abcrecipientpublickey").unwrap();
        // Round-trip parse — must be valid TOML.
        let parsed: toml::Table = toml::from_str(&result).unwrap();
        let secrets = parsed.get("secrets").and_then(|v| v.as_table()).unwrap();
        let recipients = secrets
            .get("recipients")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].as_str(), Some("age1abcrecipientpublickey"));
    }

    #[test]
    fn append_recipient_preserves_existing_other_tables() {
        // Crude string-pasting used to put a new recipient in the
        // wrong place when other tables followed `[secrets]`.
        // toml_edit handles arbitrary table ordering.
        let existing = r#"
[vars]
greet = "hi"

[secrets]
recipients = ["age1machine_a"]

[ui]
icons = "ascii"
"#;
        let result = append_recipient_to_config(existing, "host b", "age1machine_b").unwrap();
        let parsed: toml::Table = toml::from_str(&result).unwrap();
        // All three tables still there.
        assert!(parsed.get("vars").is_some());
        assert!(parsed.get("secrets").is_some());
        assert!(parsed.get("ui").is_some());
        // Both recipients in the array.
        let recipients = parsed["secrets"]["recipients"].as_array().unwrap();
        assert_eq!(recipients.len(), 2);
        let pubs: Vec<&str> = recipients.iter().filter_map(|v| v.as_str()).collect();
        assert!(pubs.contains(&"age1machine_a"));
        assert!(pubs.contains(&"age1machine_b"));
    }

    #[test]
    fn append_recipient_is_idempotent_on_duplicate() {
        let existing = r#"[secrets]
recipients = ["age1same"]
"#;
        let result = append_recipient_to_config(existing, "anyone", "age1same").unwrap();
        let parsed: toml::Table = toml::from_str(&result).unwrap();
        let recipients = parsed["secrets"]["recipients"].as_array().unwrap();
        assert_eq!(recipients.len(), 1, "duplicate must not be appended twice");
    }

    #[test]
    fn append_recipient_creates_recipients_array_when_secrets_table_empty() {
        // `[secrets]` exists but no recipients yet (e.g. user hand-
        // initialised a different field first).
        let existing = r#"[secrets]
identity = "~/.config/yui/age.txt"
"#;
        let result = append_recipient_to_config(existing, "h", "age1new").unwrap();
        let parsed: toml::Table = toml::from_str(&result).unwrap();
        let secrets = parsed["secrets"].as_table().unwrap();
        assert_eq!(
            secrets["identity"].as_str(),
            Some("~/.config/yui/age.txt"),
            "existing identity field must survive"
        );
        let recipients = secrets["recipients"].as_array().unwrap();
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].as_str(), Some("age1new"));
    }

    /// Secrets feature is opt-in: an empty `[secrets] recipients`
    /// list keeps `decrypt_all` a no-op so existing repos behave
    /// exactly as before this PR.
    #[test]
    fn apply_without_recipients_skips_secret_walker() {
        let tmp = TempDir::new().unwrap();
        let (source, _target) = setup_minimal_dotfiles(&tmp);
        // No `[secrets]` block at all.
        std::fs::write(source.join("home/.bashrc"), "x").unwrap();
        // A stray `.age` file with no recipients configured: walker
        // shouldn't even open it (no identity loaded → no decrypt
        // attempt → no error).
        std::fs::write(source.join("home/some.junk.age"), b"not actually a cipher").unwrap();
        apply(Some(source.clone()), false).unwrap();
    }

    /// v0.6+: parent `.yuilink` doesn't stop the walker. A parent
    /// marker can junction the whole dir, AND a child marker can layer
    /// on extra dsts (e.g. an OS-specific alternate location).
    #[test]
    fn nested_marker_accumulates_extra_dst() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let parent_target = utf8(tmp.path().join("home"));
        let extra_target = utf8(tmp.path().join("extra"));
        std::fs::create_dir_all(source.join("home/.config/nvim")).unwrap();
        std::fs::create_dir_all(&parent_target).unwrap();
        std::fs::create_dir_all(&extra_target).unwrap();
        std::fs::write(source.join("home/.config/nvim/init.lua"), "-- nvim\n").unwrap();

        // Parent: junction the whole .config dir to <home>/.config.
        std::fs::write(
            source.join("home/.config/.yuilink"),
            format!(
                r#"
[[link]]
dst = "{}/.config"
"#,
                toml_path(&parent_target)
            ),
        )
        .unwrap();
        // Child: ALSO junction nvim/ to an extra path, but only on the
        // running OS (so the test exercises an active link).
        std::fs::write(
            source.join("home/.config/nvim/.yuilink"),
            format!(
                r#"
[[link]]
dst = "{}/nvim"
when = "{{{{ yui.os == '{}' }}}}"
"#,
                toml_path(&extra_target),
                std::env::consts::OS
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

        // Both links are present: parent's whole-.config junction reaches
        // init.lua, and the child marker added an additional path.
        assert!(parent_target.join(".config/nvim/init.lua").exists());
        assert!(extra_target.join("nvim/init.lua").exists());
    }

    /// v0.6+: `[[link]] src = "<filename>"` links a single sibling file
    /// to a custom dst, leaving the rest of the dir to default
    /// behaviour. Useful for paths like the PowerShell profile that
    /// have to live in a non-`~/.config` location on Windows.
    #[test]
    fn marker_file_link_targets_specific_file() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let parent_target = utf8(tmp.path().join("home"));
        let docs_target = utf8(tmp.path().join("docs"));
        std::fs::create_dir_all(source.join("home/.config/powershell")).unwrap();
        std::fs::create_dir_all(&parent_target).unwrap();
        std::fs::create_dir_all(&docs_target).unwrap();
        std::fs::write(
            source.join("home/.config/powershell/profile.ps1"),
            "# profile\n",
        )
        .unwrap();
        std::fs::write(source.join("home/.config/powershell/extra.txt"), "extra\n").unwrap();

        // File-level entry only — no dir-level [[link]], so the dir
        // itself still falls through to the default mount placement.
        std::fs::write(
            source.join("home/.config/powershell/.yuilink"),
            format!(
                r#"
[[link]]
src = "profile.ps1"
dst = "{}/Microsoft.PowerShell_profile.ps1"
"#,
                toml_path(&docs_target)
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

        // File-level target gets the link.
        assert!(
            docs_target
                .join("Microsoft.PowerShell_profile.ps1")
                .exists()
        );
        // Default per-file placement still happens for ALL files in the
        // dir (the marker had no dir-level [[link]] to claim coverage).
        assert!(
            parent_target
                .join(".config/powershell/profile.ps1")
                .exists()
        );
        assert!(parent_target.join(".config/powershell/extra.txt").exists());
    }

    /// File-level [[link]] errors clearly when src points at a missing
    /// file — config bug, not a silent skip.
    #[test]
    fn marker_file_link_missing_src_errors() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let parent_target = utf8(tmp.path().join("home"));
        let docs_target = utf8(tmp.path().join("docs"));
        std::fs::create_dir_all(source.join("home/.config/powershell")).unwrap();
        std::fs::create_dir_all(&parent_target).unwrap();
        std::fs::create_dir_all(&docs_target).unwrap();

        std::fs::write(
            source.join("home/.config/powershell/.yuilink"),
            format!(
                r#"
[[link]]
src = "missing.ps1"
dst = "{}/profile.ps1"
"#,
                toml_path(&docs_target)
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

        let err = apply(Some(source.clone()), false).unwrap_err();
        assert!(format!("{err:#}").contains("missing.ps1"));
    }

    // -----------------------------------------------------------------
    // unmanaged
    // -----------------------------------------------------------------

    /// `yui unmanaged` lists files in the source tree that no
    /// `[[mount.entry]]` claims. Should NOT include the repo's own
    /// scaffold (`config.toml`, `.gitignore`, `.yuilink`, `.tera`
    /// templates) — those are managed-by-yui-itself.
    #[test]
    fn unmanaged_finds_files_outside_any_mount() {
        let tmp = TempDir::new().unwrap();
        let (source, _target) = setup_minimal_dotfiles(&tmp);
        // Mount-claimed file (under `home/` per setup_minimal_dotfiles).
        std::fs::write(source.join("home/.bashrc"), "x").unwrap();
        // Truly unmanaged file at repo root.
        std::fs::write(source.join("orphan.txt"), "y").unwrap();
        std::fs::create_dir_all(source.join("notes")).unwrap();
        std::fs::write(source.join("notes/scratch.md"), "z").unwrap();

        // unmanaged() should succeed and not touch anything.
        unmanaged(Some(source.clone()), None, /* no_color */ true).unwrap();

        // Verify the helper itself classifies correctly without printing.
        let yui = YuiVars::detect(&source);
        let cfg = config::load(&source, &yui).unwrap();
        let mount_srcs: Vec<Utf8PathBuf> = cfg
            .mount
            .entry
            .iter()
            .map(|m| source.join(&m.src))
            .collect();
        let walker = paths::source_walker(&source).build();
        let mut unmanaged_paths = Vec::new();
        for entry in walker.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let p = match Utf8PathBuf::from_path_buf(entry.path().to_path_buf()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if is_repo_meta(&p, &source, &cfg.mount.marker_filename) {
                continue;
            }
            if mount_srcs.iter().any(|m| p.starts_with(m)) {
                continue;
            }
            unmanaged_paths.push(p);
        }
        let names: Vec<String> = unmanaged_paths
            .iter()
            .filter_map(|p| p.file_name().map(String::from))
            .collect();
        assert!(names.contains(&"orphan.txt".into()));
        assert!(names.contains(&"scratch.md".into()));
        assert!(!names.contains(&".bashrc".into()), "mount-claimed file");
        assert!(!names.contains(&"config.toml".into()), "repo meta");
    }

    #[test]
    fn is_repo_meta_recognises_yui_scaffold() {
        let source = Utf8Path::new("/dot");
        // Repo-root config layering — yui-owned.
        assert!(is_repo_meta(
            Utf8Path::new("/dot/config.toml"),
            source,
            ".yuilink",
        ));
        assert!(is_repo_meta(
            Utf8Path::new("/dot/config.local.toml"),
            source,
            ".yuilink",
        ));
        assert!(is_repo_meta(
            Utf8Path::new("/dot/config.linux.toml"),
            source,
            ".yuilink",
        ));
        assert!(is_repo_meta(
            Utf8Path::new("/dot/config.local.example.toml"),
            source,
            ".yuilink",
        ));
        // Repo-root .gitignore — yui manages its rendered-files section.
        assert!(is_repo_meta(
            Utf8Path::new("/dot/.gitignore"),
            source,
            ".yuilink",
        ));
        // Marker / yuiignore / *.tera — anywhere in the tree.
        assert!(is_repo_meta(
            Utf8Path::new("/dot/home/.config/foo/.yuilink"),
            source,
            ".yuilink",
        ));
        assert!(is_repo_meta(
            Utf8Path::new("/dot/home/.gitconfig.tera"),
            source,
            ".yuilink",
        ));
        // Nested config.toml is a user dotfile, NOT yui's config.
        assert!(!is_repo_meta(
            Utf8Path::new("/dot/home/.config/myapp/config.toml"),
            source,
            ".yuilink",
        ));
        // Nested .gitignore is a user dotfile too — only the
        // repo-root one is yui-managed. (PR #53 review caught
        // the original code marking every .gitignore as meta.)
        assert!(!is_repo_meta(
            Utf8Path::new("/dot/home/.config/git/.gitignore"),
            source,
            ".yuilink",
        ));
    }

    /// `unmanaged` must NOT report files under a mount entry that's
    /// inactive on the current host (e.g. `home_macos/foo` when on
    /// Linux). The raw `config.mount.entry` list — not
    /// `mount::resolve` which filters by `when` — claims those
    /// files. (PR #53 review caught the original code using
    /// `mount::resolve`.)
    #[test]
    fn unmanaged_respects_inactive_mount_entries() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home_active")).unwrap();
        std::fs::create_dir_all(source.join("home_other_os")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("home_active/.bashrc"), "active").unwrap();
        std::fs::write(source.join("home_other_os/.bashrc"), "inactive").unwrap();
        // One mount active, one with a `when` that's always false.
        let cfg = format!(
            r#"
[[mount.entry]]
src = "home_active"
dst = "{target}"

[[mount.entry]]
src = "home_other_os"
dst = "{target}"
when = "yui.os == 'definitely_not_a_real_os'"
"#,
            target = toml_path(&target)
        );
        std::fs::write(source.join("config.toml"), cfg).unwrap();

        // Replicate unmanaged()'s classification logic and verify the
        // `home_other_os/.bashrc` file is NOT listed (because the
        // when=false mount entry still owns it on principle).
        let yui = YuiVars::detect(&source);
        let cfg = config::load(&source, &yui).unwrap();
        let mount_srcs: Vec<Utf8PathBuf> = cfg
            .mount
            .entry
            .iter()
            .map(|m| source.join(&m.src))
            .collect();
        let inactive_file = source.join("home_other_os/.bashrc");
        let claimed = mount_srcs.iter().any(|m| inactive_file.starts_with(m));
        assert!(
            claimed,
            "raw config.mount.entry should claim files even under inactive mounts"
        );
    }

    // -----------------------------------------------------------------
    // diff
    // -----------------------------------------------------------------

    #[test]
    fn diff_shows_drift_skips_in_sync() {
        let tmp = TempDir::new().unwrap();
        let (source, target) = setup_minimal_dotfiles(&tmp);
        std::fs::write(source.join("home/.bashrc"), "first\nsecond\n").unwrap();
        // Sync once.
        apply(Some(source.clone()), false).unwrap();
        // Edit target — break the link, create content drift.
        std::fs::remove_file(target.join(".bashrc")).unwrap();
        std::fs::write(target.join(".bashrc"), "first\nEDITED\n").unwrap();

        // diff() should run without bailing — the drift is what it
        // exists to surface.
        diff(Some(source.clone()), None, /* no_color */ true).unwrap();
    }

    /// `read_text_for_diff` distinguishes binary (invalid UTF-8)
    /// from text and from missing — so `print_unified_diff` /
    /// `print_absorb_diff` can short-circuit instead of dumping
    /// bytes through `similar`. (PR #53 review.)
    #[test]
    fn read_text_for_diff_classifies_correctly() {
        let tmp = TempDir::new().unwrap();
        let root = utf8(tmp.path().to_path_buf());
        // Plain UTF-8.
        let txt = root.join("a.txt");
        std::fs::write(&txt, "hello\n").unwrap();
        match read_text_for_diff(&txt) {
            DiffSide::Text(s) => assert_eq!(s, "hello\n"),
            DiffSide::Binary => panic!("text file misclassified as binary"),
        }
        // Invalid UTF-8 bytes.
        let bin = root.join("b.bin");
        std::fs::write(&bin, [0xff, 0xfe, 0x00, 0xff]).unwrap();
        assert!(matches!(read_text_for_diff(&bin), DiffSide::Binary));
        // Missing file collapses to empty Text — graceful for races.
        let missing = root.join("missing.txt");
        match read_text_for_diff(&missing) {
            DiffSide::Text(s) => assert!(s.is_empty()),
            DiffSide::Binary => panic!("missing file misclassified as binary"),
        }
    }

    /// `yui diff` for a render-drifted template must diff the
    /// **rendered output** against the on-disk file, not the raw
    /// `.tera` source — otherwise Tera's `{{ }}` syntax shows up
    /// as drift. The fix exposes `render::render_to_string` for
    /// `print_unified_diff` to compute the expected content.
    /// (PR #53 review caught this.)
    #[test]
    fn diff_render_drift_uses_rendered_output_not_raw_template() {
        let tmp = TempDir::new().unwrap();
        let (source, _target) = setup_minimal_dotfiles(&tmp);
        // Template renders `os = linux` (or whatever the host is);
        // the on-disk rendered file is stale ("os = ancient").
        std::fs::write(source.join("home/note.tera"), "os = {{ yui.os }}\n").unwrap();
        std::fs::write(source.join("home/note"), "os = ancient\n").unwrap();
        // The renderer should produce the expected new content.
        let yui = YuiVars::detect(&source);
        let cfg = config::load(&source, &yui).unwrap();
        let rendered =
            render::render_to_string(&source.join("home/note.tera"), &source, &cfg, &yui)
                .unwrap()
                .expect("template should render on this host");
        assert!(rendered.starts_with("os = "));
        assert!(
            !rendered.contains("{{"),
            "rendered output must not contain raw Tera tags"
        );
    }

    /// Regression for the path-resolution bug coderabbitai flagged
    /// on PR #53: `StatusItem.src` is a *relative-for-display*
    /// path, so reading it directly during diff rendering would
    /// resolve against the caller's cwd — empty file, wrong file,
    /// or NotFound. `resolve_diff_src` re-absolutizes against the
    /// source root for `Link(_)` rows, leaves `RenderDrift` rows
    /// alone (those already carry absolute `.tera` paths).
    #[test]
    fn resolve_diff_src_absolutizes_link_rows() {
        let source = Utf8Path::new("/dot");
        let link_item = StatusItem {
            src: Utf8PathBuf::from("home/.bashrc"),
            dst: Utf8PathBuf::from("/h/u/.bashrc"),
            state: StatusState::Link(absorb::AbsorbDecision::AutoAbsorb),
        };
        assert_eq!(
            resolve_diff_src(&link_item, source),
            Utf8PathBuf::from("/dot/home/.bashrc"),
        );
        let render_item = StatusItem {
            src: Utf8PathBuf::from("/dot/home/foo.tera"),
            dst: Utf8PathBuf::from("/dot/home/foo"),
            state: StatusState::RenderDrift,
        };
        assert_eq!(
            resolve_diff_src(&render_item, source),
            Utf8PathBuf::from("/dot/home/foo.tera"),
        );
    }

    #[test]
    fn diff_classifier_skips_uninteresting_states() {
        use absorb::AbsorbDecision::*;
        // Neither InSync nor Restore nor RelinkOnly is worth diffing.
        assert!(!diff_worth_printing(&StatusState::Link(InSync)));
        assert!(!diff_worth_printing(&StatusState::Link(Restore)));
        assert!(!diff_worth_printing(&StatusState::Link(RelinkOnly)));
        // Anything content-divergent is.
        assert!(diff_worth_printing(&StatusState::Link(AutoAbsorb)));
        assert!(diff_worth_printing(&StatusState::Link(NeedsConfirm)));
        assert!(diff_worth_printing(&StatusState::RenderDrift));
    }

    // -----------------------------------------------------------------
    // update
    // -----------------------------------------------------------------

    /// `yui update` bails out early on a dirty source tree before
    /// even shelling out to `git pull`. Easiest way to provoke that
    /// is on a fresh untracked file in a git repo, but git itself
    /// isn't always available in the test sandbox — fall back to
    /// only asserting the path that DOES run cleanly: a non-repo
    /// directory yields a clear `git: ...` error from is_clean.
    #[test]
    fn update_errors_when_source_is_not_a_git_repo() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("config.toml"), "").unwrap();
        // No `.git` here — is_clean should bail.
        let err = update(Some(source), false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a git repository")
                || msg.contains("uncommitted")
                || msg.contains("git"),
            "unexpected error: {msg}",
        );
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

    // -----------------------------------------------------------------
    // gc-backup
    // -----------------------------------------------------------------

    #[test]
    fn parse_backup_suffix_recognises_file_with_extension() {
        let dt = parse_backup_suffix("foo_20260429_143022123.yml").unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 4);
        assert_eq!(dt.day(), 29);
        assert_eq!(dt.hour(), 14);
        assert_eq!(dt.minute(), 30);
        assert_eq!(dt.second(), 22);
    }

    #[test]
    fn parse_backup_suffix_recognises_dotfile_no_extension() {
        let dt = parse_backup_suffix(".gitconfig_20260429_143022123").unwrap();
        assert_eq!(dt.year(), 2026);
    }

    #[test]
    fn parse_backup_suffix_recognises_directory_form() {
        let dt = parse_backup_suffix("nvim_20260429_143022123").unwrap();
        assert_eq!(dt.day(), 29);
    }

    #[test]
    fn parse_backup_suffix_recognises_multi_dot_filename() {
        // archive.tar.gz_<ts>.gz round-trips back through the rsplit-on-dot fallback.
        let dt = parse_backup_suffix("archive.tar.gz_20260429_143022123.gz").unwrap();
        assert_eq!(dt.month(), 4);
    }

    #[test]
    fn parse_backup_suffix_rejects_non_yui_names() {
        assert!(parse_backup_suffix("README.md").is_none());
        assert!(parse_backup_suffix("notes_2026.txt").is_none());
        assert!(parse_backup_suffix("almost_20260429_14302212").is_none()); // 17 digits
        assert!(parse_backup_suffix("almost_20260429-143022123").is_none()); // wrong sep
        // Bare timestamp with no stem is rejected (defensive — yui never produces these).
        assert!(parse_backup_suffix("_20260429_143022123").is_none());
    }

    #[test]
    fn parse_human_duration_basic_units() {
        let s = parse_human_duration("30d").unwrap();
        assert_eq!(s.get_days(), 30);
        let s = parse_human_duration("2w").unwrap();
        assert_eq!(s.get_weeks(), 2);
        let s = parse_human_duration("12h").unwrap();
        assert_eq!(s.get_hours(), 12);
        // `m` is minutes (matches what `format_age` prints), `mo` is months.
        let s = parse_human_duration("5m").unwrap();
        assert_eq!(s.get_minutes(), 5);
        let s = parse_human_duration("6mo").unwrap();
        assert_eq!(s.get_months(), 6);
        let s = parse_human_duration("1y").unwrap();
        assert_eq!(s.get_years(), 1);
    }

    #[test]
    fn parse_human_duration_case_insensitive_and_whitespace() {
        let s = parse_human_duration("  90D  ").unwrap();
        assert_eq!(s.get_days(), 90);
        let s = parse_human_duration("3WEEKS").unwrap();
        assert_eq!(s.get_weeks(), 3);
    }

    #[test]
    fn parse_human_duration_rejects_garbage() {
        assert!(parse_human_duration("").is_err());
        assert!(parse_human_duration("d30").is_err());
        assert!(parse_human_duration("30").is_err()); // no unit
        assert!(parse_human_duration("30x").is_err()); // unknown unit
        assert!(parse_human_duration("-1d").is_err()); // negative
    }

    /// Plant a real-shaped backup tree and confirm `walk_gc_backups`
    /// finds both files and dir-snapshots, treats dirs as one unit
    /// (no descent), and ignores anything without yui's suffix.
    #[test]
    fn walk_gc_backups_collects_files_and_dir_snapshots() {
        let tmp = TempDir::new().unwrap();
        let root = utf8(tmp.path().to_path_buf()).join(".yui/backup");
        std::fs::create_dir_all(root.join("C/Users/u/.config")).unwrap();
        // File-style backup.
        std::fs::write(
            root.join("C/Users/u/.config/foo_20260429_143022123.yml"),
            "old yml",
        )
        .unwrap();
        // Dir-style backup with internal files (must not be surfaced individually).
        std::fs::create_dir_all(root.join("C/Users/u/nvim_20260101_000000000/lua")).unwrap();
        std::fs::write(
            root.join("C/Users/u/nvim_20260101_000000000/init.lua"),
            "ok",
        )
        .unwrap();
        std::fs::write(
            root.join("C/Users/u/nvim_20260101_000000000/lua/x.lua"),
            "kk",
        )
        .unwrap();
        // User-dropped file with no yui suffix — must stay out of the survey.
        std::fs::write(root.join("C/Users/u/.config/README.md"), "user note").unwrap();

        let entries = walk_gc_backups(&root).unwrap();
        assert_eq!(entries.len(), 2, "two backup roots, not three");
        let kinds: Vec<_> = entries.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&BackupKind::File));
        assert!(kinds.contains(&BackupKind::Dir));
        // Dir-size aggregates contents.
        let dir_entry = entries.iter().find(|e| e.kind == BackupKind::Dir).unwrap();
        assert!(dir_entry.size_bytes >= 4); // "ok" + "kk"
    }

    #[test]
    fn cleanup_empty_parents_stops_at_root_and_at_non_empty() {
        let tmp = TempDir::new().unwrap();
        let root = utf8(tmp.path().to_path_buf()).join(".yui/backup");
        std::fs::create_dir_all(root.join("C/Users/u/.config")).unwrap();
        std::fs::write(root.join("C/Users/u/sibling_keep"), "x").unwrap();

        // Pretend we just deleted everything under .config/, the parent
        // is now empty and walks up — but Users/ has `sibling_keep` so
        // we must stop there. .yui/backup itself must never be removed.
        cleanup_empty_parents(&root.join("C/Users/u/.config"), &root);

        assert!(!root.join("C/Users/u/.config").exists(), "empty leaf gone");
        assert!(root.join("C/Users/u").exists(), "stops at non-empty parent");
        assert!(root.exists(), "backup root preserved");
    }

    /// Survey mode (no `--older-than`) lists everything and deletes nothing.
    #[test]
    fn gc_backup_survey_keeps_all_entries() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(source.join(".yui/backup")).unwrap();
        std::fs::write(source.join("config.toml"), "").unwrap();
        let backup = source.join(".yui/backup");
        std::fs::write(backup.join("a_20260101_000000000.txt"), "old").unwrap();
        std::fs::write(backup.join("b_20260415_120000000.txt"), "fresh").unwrap();

        gc_backup(Some(source.clone()), None, false, None, true).unwrap();

        // Both still present.
        assert!(backup.join("a_20260101_000000000.txt").exists());
        assert!(backup.join("b_20260415_120000000.txt").exists());
    }

    /// Prune mode deletes entries strictly older than the cutoff and
    /// leaves newer ones plus user-dropped files alone.
    #[test]
    fn gc_backup_prune_removes_old_files_only() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(source.join(".yui/backup/sub")).unwrap();
        std::fs::write(source.join("config.toml"), "").unwrap();
        let backup = source.join(".yui/backup");

        // Far-past file (will be older than 30d unless this test runs in 2026-01).
        std::fs::write(backup.join("sub/old_20200101_000000000.txt"), "old").unwrap();
        // Tomorrow → ts > now → never older than any positive cutoff.
        let tomorrow = jiff::Zoned::now()
            .checked_add(jiff::Span::new().days(1))
            .unwrap();
        let bdt = jiff::fmt::strtime::BrokenDownTime::from(&tomorrow);
        let future_ts = bdt.to_string("%Y%m%d_%H%M%S%3f").unwrap();
        std::fs::write(backup.join(format!("fresh_{future_ts}.txt")), "fresh").unwrap();
        // User-dropped file — not in yui shape.
        std::fs::write(backup.join("notes.md"), "mine").unwrap();

        gc_backup(Some(source.clone()), Some("30d".into()), false, None, true).unwrap();

        assert!(!backup.join("sub/old_20200101_000000000.txt").exists());
        // Empty parent dir got cleaned up too.
        assert!(!backup.join("sub").exists(), "empty parent removed");
        // Backup root itself is preserved even after losing children.
        assert!(backup.exists());
        assert!(backup.join(format!("fresh_{future_ts}.txt")).exists());
        assert!(backup.join("notes.md").exists(), "user file untouched");
    }

    /// `--dry-run` prints the same set but mutates nothing.
    #[test]
    fn gc_backup_dry_run_does_not_delete() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(source.join(".yui/backup")).unwrap();
        std::fs::write(source.join("config.toml"), "").unwrap();
        let backup = source.join(".yui/backup");
        std::fs::write(backup.join("old_20200101_000000000.txt"), "old").unwrap();

        gc_backup(Some(source.clone()), Some("30d".into()), true, None, true).unwrap();

        assert!(
            backup.join("old_20200101_000000000.txt").exists(),
            "dry-run keeps everything in place"
        );
    }

    /// Dir-snapshots are removed wholesale (no per-file judgment) and
    /// the now-empty mirror parents collapse up to (but not past) the
    /// backup root.
    #[test]
    fn gc_backup_prune_handles_directory_snapshot() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        std::fs::create_dir_all(source.join(".yui/backup/mirror/u")).unwrap();
        std::fs::write(source.join("config.toml"), "").unwrap();
        let backup = source.join(".yui/backup");
        let snap = backup.join("mirror/u/nvim_20200101_000000000");
        std::fs::create_dir_all(snap.join("lua")).unwrap();
        std::fs::write(snap.join("init.lua"), "x").unwrap();
        std::fs::write(snap.join("lua/y.lua"), "y").unwrap();

        gc_backup(Some(source.clone()), Some("30d".into()), false, None, true).unwrap();

        assert!(!snap.exists(), "dir snapshot removed wholesale");
        assert!(!backup.join("mirror").exists(), "empty mirror chain pruned");
        assert!(backup.exists(), "backup root preserved");
    }

    /// Build a no-op `ApplyCtx` over a `TempDir`. The returned tuple
    /// owns the `Config` + paths so the borrow in `ApplyCtx` is valid
    /// for the test scope. Callers can mutate the `Cell` fields in
    /// place.
    fn ctx_for_test(tmp: &TempDir) -> (Config, Utf8PathBuf, Utf8PathBuf) {
        let source = utf8(tmp.path().join("src"));
        let backup_root = source.join(".yui/backup");
        std::fs::create_dir_all(&source).unwrap();
        let cfg = Config::default();
        (cfg, source, backup_root)
    }

    #[test]
    fn prompt_anomaly_short_circuits_on_sticky_choice() {
        // The whole point of sticky: once the user picks an `[A]`/`[O]`/`[S]`
        // "all" option, every following anomaly applies that choice without
        // re-prompting. We verify by pre-setting the cell and calling the
        // prompt with stdin/stderr that would otherwise prompt.
        let tmp = TempDir::new().unwrap();
        let (cfg, source, backup_root) = ctx_for_test(&tmp);
        let src_file = source.join("a");
        let dst_file = utf8(tmp.path().join("dst"));
        std::fs::write(&src_file, "X").unwrap();
        std::fs::write(&dst_file, "Y").unwrap();

        let ctx = ApplyCtx {
            config: &cfg,
            source: &source,
            file_mode: resolve_file_mode(cfg.link.file_mode),
            dir_mode: resolve_dir_mode(cfg.link.dir_mode),
            backup_root: &backup_root,
            dry_run: false,
            sticky_anomaly: Cell::new(Some(AnomalyChoice::Overwrite)),
            quit_requested: Cell::new(false),
        };

        let got = prompt_anomaly(&ctx, &src_file, &dst_file, "test").unwrap();
        assert_eq!(got, AnomalyChoice::Overwrite);
    }

    #[test]
    fn overwrite_source_into_target_replaces_target_and_backs_up() {
        // `[o]verwrite`'s contract: the user keeps source's content and
        // discards target's. After the call target reflects source, and
        // target's old content is preserved under backup so it is
        // recoverable.
        let tmp = TempDir::new().unwrap();
        let (cfg, source, backup_root) = ctx_for_test(&tmp);
        let src_file = source.join("a");
        let dst_file = utf8(tmp.path().join("dst"));
        std::fs::write(&src_file, "from source").unwrap();
        std::fs::write(&dst_file, "diverged target content").unwrap();

        let ctx = ApplyCtx {
            config: &cfg,
            source: &source,
            file_mode: resolve_file_mode(cfg.link.file_mode),
            dir_mode: resolve_dir_mode(cfg.link.dir_mode),
            backup_root: &backup_root,
            dry_run: false,
            sticky_anomaly: Cell::new(None),
            quit_requested: Cell::new(false),
        };

        overwrite_source_into_target(&src_file, &dst_file, &ctx).unwrap();

        // Target now matches source.
        assert_eq!(std::fs::read_to_string(&dst_file).unwrap(), "from source");
        // Source untouched.
        assert_eq!(std::fs::read_to_string(&src_file).unwrap(), "from source");
        // The diverged target content survives in backup.
        let mut found_old = false;
        for entry in walkdir(&backup_root) {
            if let Ok(s) = std::fs::read_to_string(&entry) {
                if s == "diverged target content" {
                    found_old = true;
                    break;
                }
            }
        }
        assert!(
            found_old,
            "expected backup containing target's diverged content"
        );
    }

    #[test]
    fn link_file_with_backup_short_circuits_when_quit_requested() {
        // After `[q]uit` the walker keeps iterating but `quit_requested`
        // makes every link op return Ok(()) without touching disk. We
        // set up a clear anomaly (target older + content differs +
        // on_anomaly=force, which would otherwise absorb) and verify
        // nothing changed.
        let tmp = TempDir::new().unwrap();
        let (mut cfg, source, backup_root) = ctx_for_test(&tmp);
        cfg.absorb.on_anomaly = crate::config::AnomalyAction::Force;

        let src_file = source.join("a");
        let dst_file = utf8(tmp.path().join("dst"));
        let now = std::time::SystemTime::now();
        let past = now - std::time::Duration::from_secs(120);
        write_with_mtime(&dst_file, "target old", past);
        write_with_mtime(&src_file, "source new", now);
        let dst_before = std::fs::read_to_string(&dst_file).unwrap();
        let src_before = std::fs::read_to_string(&src_file).unwrap();

        let ctx = ApplyCtx {
            config: &cfg,
            source: &source,
            file_mode: resolve_file_mode(cfg.link.file_mode),
            dir_mode: resolve_dir_mode(cfg.link.dir_mode),
            backup_root: &backup_root,
            dry_run: false,
            sticky_anomaly: Cell::new(None),
            quit_requested: Cell::new(true),
        };

        link_file_with_backup(&src_file, &dst_file, &ctx).unwrap();

        assert_eq!(std::fs::read_to_string(&dst_file).unwrap(), dst_before);
        assert_eq!(std::fs::read_to_string(&src_file).unwrap(), src_before);
        assert!(
            !backup_root.exists() || walkdir(&backup_root).is_empty(),
            "no backup should be produced when quit is requested"
        );
    }
}
