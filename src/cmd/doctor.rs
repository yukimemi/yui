use super::*;
use crate::config::{self, IconsMode};
use crate::icons::Icons;
use crate::paths;
use crate::vars::YuiVars;
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

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
