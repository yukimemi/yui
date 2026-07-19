use super::*;
use crate::config::{self, Config};
use crate::link::{resolve_dir_mode, resolve_file_mode};
use crate::marker::{self, MarkerSpec};
use crate::paths;
use crate::template;
use crate::vars::YuiVars;
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use std::cell::Cell;
use teravars::Context as TeraContext;
use tracing::{info, warn};

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
    let mut config = config::load(&source, &yui)?;
    config.absorb.on_anomaly = config::AnomalyAction::Force;

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
    if target.is_dir() {
        absorb_target_dir_into_source(&src_path, &target, &ctx)
    } else {
        absorb_target_into_source(&src_path, &target, &ctx)
    }
}

/// Stderr-print a unified diff between `src` (file or dir) and `dst`
/// using `similar`. Falls back to a one-line description when one
/// side is a directory or content isn't valid UTF-8 — we'd rather
/// say "binary file differs" than spew bytes through `similar`.
pub(crate) fn print_absorb_diff(src: &Utf8Path, dst: &Utf8Path) {
    use owo_colors::OwoColorize as _;
    use std::io::IsTerminal;

    // Honor the de-facto NO_COLOR convention (https://no-color.org/) —
    // any non-empty value disables colorisation, even on a TTY.
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();

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
            // Honor the ignore rules even on manual absorb — if
            // you've ignored a path, you've explicitly opted out of
            // yui's managing it. One-shot stack walk along the
            // candidate's parents picks up nested ignore files too.
            //
            // `is_dir` comes from `target`, not `candidate`: absorbing
            // something that doesn't exist in source yet is the normal
            // case, and a non-existent `candidate.is_dir()` is always
            // false — which would silently skip every directory-only
            // pattern (`sessions/`, `cache/`) and absorb the very trees
            // the ignore rules exclude. `target` is the thing being
            // absorbed, so it's the authority on directory-ness.
            if paths::is_ignored_at(
                source,
                &candidate,
                target.is_dir(),
                config.mount.respect_gitignore,
            )? {
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
