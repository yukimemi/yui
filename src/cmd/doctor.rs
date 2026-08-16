use super::*;
use crate::config::{self, Config, IconsMode, MountStrategy};
use crate::icons::Icons;
use crate::link::{EffectiveDirMode, resolve_dir_mode};
use crate::links::LinkPlan;
use crate::mount;
use crate::paths;
use crate::template;
use crate::vars::YuiVars;
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeSet;

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
    if let (Ok(s), Some(c)) = (&resolved_source, cfg) {
        // Resilience principle: a config yui can't fully resolve
        // (unrenderable mount dst, bad `[[link]]` src) must not take
        // the rest of doctor down — degrade this one probe instead.
        match undeclared_dir_link_probes(s, c, &yui) {
            Ok(found) => probes.extend(found),
            Err(e) => probes.push(Probe::warn("dir links", format!("check skipped — {e}"))),
        }
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

/// Target-side directory links that no `[[link]]` declaration asks for.
///
/// A junction / symlink pointing back into the source tree is
/// load-bearing: *everything* under the source directory — tracked or
/// not — is visible from the target through it. When nothing declares
/// it, nothing recreates it, and no other command notices: both sides
/// resolve to the same inode *through* that very link, so
/// [`crate::absorb::classify`] reports the files inside as `in-sync`
/// forever. Remove the link (new machine, `yui unlink`, a manual
/// cleanup) and `apply` faithfully rebuilds the target as a plain
/// directory holding per-file links of only the tracked files; the
/// rest of the source directory silently stops being visible from the
/// target side.
///
/// Warn, never error: the link is not broken *now*, it is
/// unreproducible — and doctor exists to say that before `apply` does
/// something surprising.
pub(crate) fn undeclared_dir_link_probes(
    source: &Utf8Path,
    config: &Config,
    yui: &YuiVars,
) -> Result<Vec<Probe>> {
    let mut engine = template::Engine::new();
    let tera_ctx = template::template_context(yui, &config.vars);
    let mounts = mount::resolve(
        source,
        &config.mount.entry,
        config.mount.default_strategy,
        &mut engine,
        &tera_ctx,
    )?;
    let plan = LinkPlan::from_config(source, &config.link)?;
    let marker_filename = &config.mount.marker_filename;

    // Declaration roots: `$DOTFILES` plus any mount `src` living
    // outside it (an absolute `src` is how a separate private clone
    // participates), so a `.yuilink` out there still counts as a
    // declaration.
    let mut roots: Vec<Utf8PathBuf> = vec![source.to_path_buf()];
    for m in &mounts {
        if !roots.iter().any(|r| m.src.starts_with(r)) {
            roots.push(m.src.clone());
        }
    }

    // Every target path some declaration names. Rendered and
    // tilde-expanded exactly like the apply walk does it, so the
    // comparison below is against the path apply would create.
    let mut declared: BTreeSet<Utf8PathBuf> = BTreeSet::new();
    for root in &roots {
        for dir in plan.declared_dirs(root, marker_filename) {
            // Markers only count where the mount honours them — same
            // gate the apply walk applies.
            let honor_markers = mounts
                .iter()
                .find(|m| dir.starts_with(&m.src))
                .is_none_or(|m| m.strategy == MountStrategy::Marker);
            let spec = plan.dir_spec(&dir, marker_filename, honor_markers)?;
            if spec.passthrough {
                for m in &mounts {
                    if let Ok(rel) = dir.strip_prefix(&m.src) {
                        declared.insert(paths::normalize(&m.dst.join(rel)));
                    }
                }
            }
            for link in &spec.links {
                if let Some(when) = &link.when {
                    if !template::eval_truthy(when, &mut engine, &tera_ctx)? {
                        continue;
                    }
                }
                let rendered = engine.render(&link.dst, &tera_ctx)?;
                declared.insert(paths::normalize(&paths::expand_tilde(rendered.trim())));
            }
        }
    }

    // A target link only concerns yui when it resolves back into a
    // tree yui manages; an unrelated symlink (`/home/u` → `/mnt/u`)
    // is somebody else's business.
    let canonical_roots: Vec<std::path::PathBuf> = roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();

    // Name the mechanism the *config* asks for, not the platform
    // default: `[mount] dir_mode = "symlink"` is opt-in on Windows, and
    // a warning that says "junction" about a symlink misdescribes both
    // what is on disk and what apply would rebuild.
    let kind = match resolve_dir_mode(config.mount.dir_mode) {
        EffectiveDirMode::Junction => "junction",
        EffectiveDirMode::Symlink => "symlink",
    };
    let mut probes = Vec::new();
    for m in &mounts {
        if !m.src.is_dir() {
            continue;
        }
        for ent in paths::source_walker(&m.src).build() {
            let Ok(ent) = ent else { continue };
            if !ent.file_type().is_some_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(src_dir) = Utf8PathBuf::from_path_buf(ent.into_path()) else {
                continue;
            };
            let Ok(rel) = src_dir.strip_prefix(&m.src) else {
                continue;
            };
            let dst = paths::normalize(&m.dst.join(rel));
            if declared.contains(&dst) {
                continue;
            }
            let is_link =
                std::fs::symlink_metadata(&dst).is_ok_and(|md| md.file_type().is_symlink());
            if !is_link {
                continue;
            }
            let Ok(canonical) = std::fs::canonicalize(&dst) else {
                continue;
            };
            if !canonical_roots.iter().any(|r| canonical.starts_with(r)) {
                continue;
            }
            // Gitignore is only layered in here, on the handful of
            // candidates that got this far: `source_walker` honours
            // `.yuiignore` itself, and rebuilding the gitignore
            // matchers for every directory in the tree would cost far
            // more than it can ever save.
            if paths::is_ignored_at(source, &src_dir, true, config.mount.respect_gitignore)? {
                continue;
            }
            probes.push(Probe::warn(
                "undeclared dir link",
                format!(
                    "{dst} → {src_dir} — the target is a {kind} into the source tree \
                     but no [[link]] declares it; apply would rebuild it as a plain \
                     directory with per-file links"
                ),
            ));
        }
    }

    if probes.is_empty() {
        probes.push(Probe::ok(
            "dir links",
            "every target-side directory link is declared",
        ));
    }
    Ok(probes)
}

#[derive(Debug)]
pub(crate) enum Probe {
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
