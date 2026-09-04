use super::*;
use crate::config::{self, Config};
use crate::link::{resolve_dir_mode, resolve_file_mode};
use crate::links::{LinkMode, LinkPlan};
use crate::paths;
use crate::template;
use crate::vars::YuiVars;
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use std::cell::{Cell, RefCell};
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
/// path "owns" the given target. Errors loudly if no mount claims it,
/// unless `to` names a source-relative directory to declare on the fly
/// (see [`create_marker_for`]).
pub fn absorb(
    source: Option<Utf8PathBuf>,
    target: Utf8PathBuf,
    to: Option<Utf8PathBuf>,
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

    let found = find_source_for_target(&source, &config, &target, &mut engine, &tera_ctx)?;

    let (target_match, pending_marker) = match (found, to) {
        (Some(m), None) => (m, None),
        (Some(m), Some(rel)) if !m.specific => {
            // Only the generic mount-derived fallback claims this
            // target — no explicit `[[link]]`/`.yuilink` declaration
            // does. That's exactly what `--to` exists to override with
            // a new, more specific declaration.
            let (tm, pending) =
                plan_marker_for(&source, &rel, &target, &config.mount.marker_filename)?;
            (tm, Some(pending))
        }
        (Some(m), Some(rel)) => {
            // A *specific* declaration already claims this target.
            // `--to` only creates a new declaration, it never rewrites
            // an existing one — silently ignoring a mismatch would
            // absorb into a directory the user didn't ask for; bail
            // instead so they can drop `--to` or fix the conflicting
            // declaration by hand.
            let declared = paths::normalize(&source.join(&rel));
            if paths::normalize(&m.src) != declared {
                anyhow::bail!(
                    "target {target} is already claimed by a declaration pointing at \
                     {}; --to {rel} disagrees — drop --to or fix the existing declaration",
                    m.src
                );
            }
            (m, None)
        }
        (None, None) => anyhow::bail!(
            "no mount entry / .yuilink override claims target {target}; \
                 pass a path inside a known dst, or --to SRC to declare one"
        ),
        (None, Some(rel)) => {
            let (tm, pending) =
                plan_marker_for(&source, &rel, &target, &config.mount.marker_filename)?;
            (tm, Some(pending))
        }
    };
    let TargetMatch {
        src: src_path,
        mode,
        specific: _,
    } = target_match;

    info!("source for {target}: {src_path}");

    // Show the diff before *any* action. For text files we render a
    // unified diff against `similar`; for dirs / binaries we just
    // surface a one-liner so the user knows what they're about to
    // overwrite without dumping garbage to the terminal.
    print_absorb_diff(&src_path, &target);

    if dry_run {
        if let Some(pending) = &pending_marker {
            info!("[dry-run] would create {}", pending.marker_path);
        }
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

    // Only now — past both the dry-run short-circuit and the
    // confirmation gate — does `--to` actually touch the source repo.
    // Answering "no" (or `--dry-run`) must leave no trace.
    if let Some(pending) = &pending_marker {
        write_marker(pending)?;
    }

    let backup_root = source.join(&config.backup.dir);
    let plan = LinkPlan::from_config(&source, &config.link)?;
    let ctx = ApplyCtx {
        config: &config,
        plan: &plan,
        file_mode: resolve_file_mode(config.mount.file_mode),
        dir_mode: resolve_dir_mode(config.mount.dir_mode),
        backup_root: &backup_root,
        dry_run: false,
        sticky_anomaly: Cell::new(None),
        quit_requested: Cell::new(false),
        // Manual absorb bypasses the `require_clean_git` policy
        // entirely (see below), so this only satisfies the shared type.
        source_clean: true,
        unresolved: RefCell::new(Vec::new()),
    };

    // Manual absorb is an explicit user request — bypass `auto`,
    // `require_clean_git`, and `on_anomaly` policy entirely. The
    // mechanism still follows the declaration that claims this target:
    // relinking with the global mode would quietly undo a per-entry
    // `mode`, and `classify` compares by identity rather than
    // mechanism, so a later `apply` would see InSync and never put it
    // back.
    if target.is_dir() {
        absorb_target_dir_into_source(&src_path, &target, &ctx, ctx.dir_mode_for(mode))
    } else {
        absorb_target_into_source(&src_path, &target, &ctx, ctx.file_mode_for(mode))
    }
}

/// A `[[link]]` marker computed for `target` but not yet written —
/// [`plan_marker_for`] validates and renders everything that can fail
/// or that's worth logging before any confirmation gate; [`write_marker`]
/// performs the actual filesystem mutation once `absorb` has confirmed
/// it's really proceeding (past `--dry-run` and the y/N prompt).
struct PendingMarker {
    marker_dir: Utf8PathBuf,
    marker_path: Utf8PathBuf,
    content: String,
}

/// Plan a brand-new `[[link]]` marker for `target` at
/// `<source>/<rel>/.yuilink`: validate, render its content, and hand
/// back both the [`TargetMatch`] `absorb` should use and the
/// [`PendingMarker`] to write once it's safe to mutate the source
/// repo. Performs no filesystem writes itself — `dry-run` and a
/// declined confirmation must leave no trace.
///
/// Only supports directory targets — the realistic case is a whole
/// `%APPDATA%`/`$XDG_CONFIG_HOME`-style app-data directory that isn't
/// under source yet. A single-file target would need a `rel` inside
/// the declaration that names the file, which is ambiguous to invent
/// on the fly; write that `[[link]]` by hand instead.
///
/// `dst` is templatized against known env-var roots (`%APPDATA%`,
/// `$XDG_CONFIG_HOME`, `$HOME`, …) via [`templatize_dst`] so the
/// marker stays portable across machines/users, same as the
/// hand-written markers elsewhere in this tree (e.g.
/// `home/.config/shoka/.yuilink`). Falls back to the literal absolute
/// path — with a loud warning — when nothing matches.
fn plan_marker_for(
    source: &Utf8Path,
    rel: &Utf8Path,
    target: &Utf8Path,
    marker_filename: &str,
) -> Result<(TargetMatch, PendingMarker)> {
    if !target.is_dir() {
        anyhow::bail!(
            "--to only supports directory targets ({target} is a file); \
             write a `[[link]]` entry by hand for a single file"
        );
    }
    crate::links::validate_src(rel, "--to")?;

    let marker_dir = paths::normalize(&source.join(rel));
    let marker_path = marker_dir.join(marker_filename);
    if marker_path.is_file() {
        anyhow::bail!(
            "{marker_path} already exists but doesn't claim {target}; \
             fix it by hand instead of overwriting it"
        );
    }

    let (dst, matched_var) = templatize_dst(target);
    match matched_var {
        Some(name) => info!("--to {rel}: templatizing {target} via env(name='{name}')"),
        None => warn!(
            "--to {rel}: {target} doesn't fall under a known env-var root \
             (APPDATA/LOCALAPPDATA/USERPROFILE/XDG_*/HOME) — writing it as a \
             literal path, which won't portable across machines/users. \
             Edit {marker_path} by hand to templatize it."
        ),
    }

    // Serialize `dst` through `toml::Value` rather than a hand-rolled
    // `"{dst}"` interpolation — a directory name containing `"` or a
    // backslash would otherwise write a marker that fails to parse (or
    // parses into the wrong path) the moment `apply`/`absorb` reads it
    // back.
    let dst_toml = toml::Value::String(dst).to_string();
    let content = format!(
        "[[link]]\ndst = {dst_toml}\nwhen = \"yui.os == '{}'\"\n",
        std::env::consts::OS
    );

    Ok((
        TargetMatch {
            src: marker_dir.clone(),
            mode: None,
            specific: true,
        },
        PendingMarker {
            marker_dir,
            marker_path,
            content,
        },
    ))
}

/// Actually create `pending`'s directory and write its `.yuilink`
/// marker. Split out of [`plan_marker_for`] so `absorb` can call it
/// only after `--dry-run` and the confirmation prompt have both
/// cleared.
fn write_marker(pending: &PendingMarker) -> Result<()> {
    std::fs::create_dir_all(&pending.marker_dir)?;
    std::fs::write(&pending.marker_path, &pending.content)?;
    info!("created {}", pending.marker_path);
    Ok(())
}

/// Rewrite `target`'s absolute path as a Tera `env(...)` call rooted at
/// whichever known env var it falls under, longest/most-specific match
/// first. Returns the matched var name too, purely for the info/warn
/// log in [`plan_marker_for`].
fn templatize_dst(target: &Utf8Path) -> (String, Option<&'static str>) {
    templatize_dst_with(target, |name| std::env::var(name).ok())
}

/// Core of [`templatize_dst`], taking the env lookup as a parameter so
/// unit tests can exercise the path-matching logic against fake roots
/// instead of mutating process-wide `HOME`/`USERPROFILE`/`APPDATA` —
/// vars several other tests (`resolve_source`, `expand_tilde`, …) also
/// depend on for real, so contending over them would be flaky.
///
/// Requires a path-separator boundary right after the matched root —
/// a bare string-prefix test would let `HOME=/home/alice` swallow
/// `/home/alice2/app` as `{{ env(name='HOME') }}/2/app`, silently
/// rewriting the marker to point somewhere `apply` would later manage
/// a directory the absorb never touched. `target == root` (no
/// trailing component at all) also counts as a boundary match.
fn templatize_dst_with(
    target: &Utf8Path,
    lookup: impl Fn(&str) -> Option<String>,
) -> (String, Option<&'static str>) {
    let windows = std::env::consts::OS == "windows";
    let candidates: &[&str] = if windows {
        &["LOCALAPPDATA", "APPDATA", "USERPROFILE"]
    } else {
        &["XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "HOME"]
    };

    let target_str = target.as_str();
    for name in candidates {
        let Some(val) = lookup(name) else {
            continue;
        };
        let val = val.trim_end_matches(['/', '\\']);
        if val.is_empty() {
            continue;
        }
        let prefix_matches = if windows {
            target_str.len() >= val.len() && target_str[..val.len()].eq_ignore_ascii_case(val)
        } else {
            target_str.len() >= val.len() && target_str.starts_with(val)
        };
        // `target_str.len() >= val.len()` above guarantees this index
        // is in bounds when lengths differ; byte indexing (not `&str`
        // slicing) needs no char-boundary alignment. An exact-length
        // match (`target == root`) has no trailing byte to check —
        // the root itself is a valid boundary.
        let boundary = prefix_matches
            && (target_str.len() == val.len()
                || matches!(target_str.as_bytes()[val.len()], b'/' | b'\\'));
        if !boundary {
            continue;
        }
        let rest = target_str[val.len()..]
            .trim_start_matches(['/', '\\'])
            .replace('\\', "/");
        let dst = if rest.is_empty() {
            format!("{{{{ env(name='{name}') }}}}")
        } else {
            format!("{{{{ env(name='{name}') }}}}/{rest}")
        };
        return (dst, Some(name));
    }
    (target_str.replace('\\', "/"), None)
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

/// What claims a target: the source path it maps back to, the
/// mechanism the claiming declaration asked for (`None` = whatever
/// `[mount]` says), and whether the match came from an explicit
/// `[[link]]`/`.yuilink` declaration (`specific`) rather than the
/// generic mount-derived fallback. `--to` only needs to agree with a
/// `specific` match — a generic-mount match is exactly what `--to` is
/// meant to override with a new, more specific declaration.
pub(crate) struct TargetMatch {
    pub(crate) src: Utf8PathBuf,
    pub(crate) mode: Option<LinkMode>,
    pub(crate) specific: bool,
}

/// Walk mount entries + every `[[link]]` declaration (central table and
/// `.yuilink` markers) to find the source file/dir that the given target
/// maps back to. Returns `None` when nothing claims the path.
fn find_source_for_target(
    source: &Utf8Path,
    config: &Config,
    target: &Utf8Path,
    engine: &mut template::Engine,
    tera_ctx: &TeraContext,
) -> Result<Option<TargetMatch>> {
    // 1. `[[link]]` declarations — render each `dst`, see if target is
    //    that dst (or nested inside a junction'd dir). Marker discovery
    //    goes through `source_walker`, which skips `.yui/` and honours
    //    nested `.yuiignore` files, so declarations under ignored
    //    subtrees stay invisible here too.
    //
    //    Checked before generic mount entries: a declared override is
    //    more specific than the mount-derived default, and the forward
    //    `apply` walk (`walk_and_link_body`) gives it the same priority
    //    — it keeps recursing past a dir-scoped ancestor link so a
    //    nested marker's override still fires. Reverse lookup has to
    //    agree, or `absorb` proposes a different source than `apply`
    //    would actually populate.
    let plan = LinkPlan::from_config(source, &config.link)?;
    let marker_filename = &config.mount.marker_filename;
    for dir in plan.declared_dirs(source, marker_filename) {
        let spec = plan.dir_spec(&dir, marker_filename, true)?;
        for link in &spec.links {
            if let Some(when) = &link.when {
                if !template::eval_truthy(when, engine, tera_ctx)? {
                    continue;
                }
            }
            let dst_str = engine.render(&link.dst, tera_ctx)?;
            let dst = paths::expand_tilde(dst_str.trim());
            // File-scoped entry: dst points at a single file, so a match
            // resolves directly to `<dir>/<rel>`. Test the target first —
            // this is a *search* over every declaration, so a stale entry
            // elsewhere in the config must not abort the lookup for the
            // path the user actually asked about. Once it matches, mirror
            // the existence check apply / status do so the message shape
            // is the same from every entry point.
            if let Some(rel) = &link.rel {
                if target != dst {
                    continue;
                }
                let file_src = dir.join(rel);
                if !file_src.is_file() {
                    anyhow::bail!("{} not found", link.describe(&dir));
                }
                return Ok(Some(TargetMatch {
                    src: file_src,
                    mode: link.mode,
                    specific: true,
                }));
            }
            if target == dst {
                return Ok(Some(TargetMatch {
                    src: dir,
                    mode: link.mode,
                    specific: true,
                }));
            }
            if let Ok(rel) = target.strip_prefix(&dst) {
                // A path *inside* a dir-scoped link: what gets relinked
                // is that file, so the directory's mechanism doesn't
                // apply to it. Fall back to the file default.
                return Ok(Some(TargetMatch {
                    src: dir.join(rel),
                    mode: None,
                    specific: true,
                }));
            }
        }
    }

    // 2. Mount entries — render dst, see if target is inside it. Falls
    //    back here only when no declared `[[link]]` claimed the target.
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
            // A mount entry carries no mechanism of its own.
            return Ok(Some(TargetMatch {
                src: candidate,
                mode: None,
                specific: false,
            }));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod templatize_dst_tests {
    use super::templatize_dst_with;
    use camino::Utf8Path;

    // `templatize_dst_with`'s candidate list differs by OS
    // (LOCALAPPDATA/APPDATA/USERPROFILE on Windows, XDG_*/HOME
    // elsewhere). Target whichever one it consults last so these
    // tests exercise the real candidate loop on every CI platform
    // instead of hardcoding a var name that's only in one list.
    fn root_var_name() -> &'static str {
        if cfg!(windows) { "USERPROFILE" } else { "HOME" }
    }

    /// The bug CodeRabbit flagged in PR #276: a bare string-prefix
    /// test lets a shorter root swallow a sibling directory that only
    /// shares a name prefix, not a real path-component boundary.
    #[test]
    fn does_not_swallow_a_sibling_with_shared_name_prefix() {
        let root = "/home/alice";
        let target = Utf8Path::new("/home/alice2/app");
        let name = root_var_name();
        let (dst, matched) = templatize_dst_with(target, |n| (n == name).then(|| root.into()));
        assert_eq!(matched, None, "must not match past a non-boundary prefix");
        assert_eq!(dst, "/home/alice2/app");
    }

    #[test]
    fn matches_at_a_real_path_boundary() {
        let root = "/home/alice";
        let target = Utf8Path::new("/home/alice/app/data");
        let name = root_var_name();
        let (dst, matched) = templatize_dst_with(target, |n| (n == name).then(|| root.into()));
        assert_eq!(matched, Some(name));
        assert_eq!(dst, format!("{{{{ env(name='{name}') }}}}/app/data"));
    }

    #[test]
    fn matches_when_target_is_exactly_the_root() {
        let root = "/home/alice";
        let target = Utf8Path::new("/home/alice");
        let name = root_var_name();
        let (dst, matched) = templatize_dst_with(target, |n| (n == name).then(|| root.into()));
        assert_eq!(matched, Some(name));
        assert_eq!(dst, format!("{{{{ env(name='{name}') }}}}"));
    }

    #[test]
    fn falls_back_to_literal_path_when_nothing_matches() {
        let target = Utf8Path::new("/opt/elsewhere/app");
        let (dst, matched) = templatize_dst_with(target, |_| None);
        assert_eq!(matched, None);
        assert_eq!(dst, "/opt/elsewhere/app");
    }
}
