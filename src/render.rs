//! Tera template rendering for `*.tera` files.
//!
//! Output goes to the **same directory** as the source `.tera` file (e.g.
//! `home/.gitconfig.tera` → `home/.gitconfig`). When `manage_gitignore` is
//! true, the rendered files are listed in a `# >>> yui rendered ... <<<`
//! managed section of `.gitignore` so they aren't committed.
//!
//! Conditional render (both honored, AND'd together when both present):
//!   - file-header: `{# yui:when EXPR #}` as the first Tera comment in the
//!     `.tera` file. Tera comments are stripped from output, so the header
//!     never bleeds into the rendered file.
//!   - config rule: `[[render.rule]] match = "<glob>", when = "<expr>"` —
//!     the glob is matched against the path relative to the source root.
//!
//! Drift policy: if the rendered file already exists with content that
//! diverges from what the template would produce now, we DO NOT overwrite
//! it. The user has likely edited the rendered file in place and needs to
//! reflect that change back into the `.tera` first. The divergence is
//! reported in `RenderReport::diverged`; `--check` treats it as fatal.

use camino::{Utf8Path, Utf8PathBuf};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use tera::Context as TeraContext;

use crate::config::{Config, RenderRule};
use crate::template::{self, Engine};
use crate::vars::YuiVars;
use crate::{Error, Result};

const GITIGNORE_BEGIN: &str = "# >>> yui rendered (auto-managed, do not edit) >>>";
const GITIGNORE_END: &str = "# <<< yui rendered (auto-managed) <<<";

#[derive(Debug, Default)]
pub struct RenderReport {
    /// Templates rendered for the first time (or after deletion).
    pub written: Vec<Utf8PathBuf>,
    /// Rendered output identical to existing file — no write needed.
    pub unchanged: Vec<Utf8PathBuf>,
    /// Skipped because file-header or config rule `when` evaluated to false.
    pub skipped_when_false: Vec<Utf8PathBuf>,
    /// Existing rendered file diverges from current template output.
    /// User must reflect the manual edit back into `.tera` before re-rendering.
    pub diverged: Vec<Utf8PathBuf>,
}

impl RenderReport {
    pub fn has_drift(&self) -> bool {
        !self.diverged.is_empty()
    }
}

pub fn render_all(
    source: &Utf8Path,
    config: &Config,
    yui: &YuiVars,
    dry_run: bool,
) -> Result<RenderReport> {
    let mut engine = Engine::new();
    let ctx = template::template_context(yui, &config.vars);
    let rules = compile_rules(&config.render.rule)?;
    let mut report = RenderReport::default();

    // Disable .gitignore / .ignore filtering: the rendered counterparts
    // we manage are themselves gitignored, and we don't want a user's
    // unrelated ignore rules (e.g. `node_modules/`) to swallow templates
    // that live deeper. `.yuiignore` will eventually do its own filtering.
    let walker = WalkBuilder::new(source)
        .hidden(false)
        .git_ignore(false)
        .ignore(false)
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let std_path = entry.path();
        let Some(name) = std_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".tera") {
            continue;
        }
        let template_path = match Utf8PathBuf::from_path_buf(std_path.to_path_buf()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        process_template(
            &template_path,
            source,
            &rules,
            &mut engine,
            &ctx,
            dry_run,
            &mut report,
        )?;
    }

    if !dry_run && config.render.manage_gitignore {
        update_gitignore(source, &collect_managed_paths(&report))?;
    }
    Ok(report)
}

struct CompiledRule {
    matcher: GlobSet,
    when: Option<String>,
}

fn compile_rules(rules: &[RenderRule]) -> Result<Vec<CompiledRule>> {
    let mut out = Vec::with_capacity(rules.len());
    for r in rules {
        let glob = Glob::new(&r.r#match)
            .map_err(|e| Error::Config(format!("render.rule.match {:?}: {e}", r.r#match)))?;
        let mut b = GlobSetBuilder::new();
        b.add(glob);
        let matcher = b
            .build()
            .map_err(|e| Error::Config(format!("globset build: {e}")))?;
        out.push(CompiledRule {
            matcher,
            when: r.when.clone(),
        });
    }
    Ok(out)
}

fn process_template(
    template_path: &Utf8Path,
    source: &Utf8Path,
    rules: &[CompiledRule],
    engine: &mut Engine,
    ctx: &TeraContext,
    dry_run: bool,
    report: &mut RenderReport,
) -> Result<()> {
    let raw = std::fs::read_to_string(template_path)
        .map_err(|e| Error::Template(format!("read {template_path}: {e}")))?;
    let target = template_target(template_path);

    // Strip any `{# yui:when EXPR #}\n` header before handing the body to
    // Tera, so a falsy header doesn't leave a stray newline at the top of
    // a successful render.
    let body_input = if let Some((expr, body)) = split_yui_when(&raw) {
        if !eval_when(expr, engine, ctx)? {
            return skip_when_false(template_path, &target, dry_run, report);
        }
        body.to_string()
    } else {
        raw
    };

    let rel = relative_to(source, template_path);
    let rel_for_match = rel.as_str().replace('\\', "/");
    for rule in rules {
        if rule.matcher.is_match(&rel_for_match) {
            if let Some(w) = &rule.when {
                if !eval_when(w, engine, ctx)? {
                    return skip_when_false(template_path, &target, dry_run, report);
                }
            }
        }
    }

    let body = engine.render(&body_input, ctx)?;

    match std::fs::read_to_string(&target) {
        Ok(existing) if existing == body => {
            report.unchanged.push(target);
            return Ok(());
        }
        Ok(_) => {
            report.diverged.push(target);
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Template(format!("read {target}: {e}"))),
    }

    if !dry_run {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &body)?;
    }
    report.written.push(target);
    Ok(())
}

/// Common path for `when=false` skips: drops any stale rendered output so
/// the link walk doesn't end up linking yesterday's render of a now-disabled
/// template. Records the skip on the report.
fn skip_when_false(
    template_path: &Utf8Path,
    target: &Utf8Path,
    dry_run: bool,
    report: &mut RenderReport,
) -> Result<()> {
    if !dry_run && target.exists() {
        std::fs::remove_file(target)
            .map_err(|e| Error::Template(format!("removing stale rendered {target}: {e}")))?;
    }
    report.skipped_when_false.push(template_path.to_path_buf());
    Ok(())
}

/// If the file begins with a `{# yui:when EXPR #}` header (after optional
/// leading whitespace) followed by an immediate newline, returns
/// `(expr, body)` where `body` is the file content with that header line
/// stripped. Otherwise None.
fn split_yui_when(raw: &str) -> Option<(&str, &str)> {
    let leading_ws = raw.len() - raw.trim_start().len();
    let after_ws = &raw[leading_ws..];
    let after_open = after_ws.strip_prefix("{#")?;
    let close = after_open.find("#}")?;
    let inside = &after_open[..close];
    let expr = inside.trim().strip_prefix("yui:when")?.trim();

    let mut body_start = leading_ws + 2 + close + 2;
    if raw[body_start..].starts_with("\r\n") {
        body_start += 2;
    } else if raw[body_start..].starts_with('\n') {
        body_start += 1;
    }
    Some((expr, &raw[body_start..]))
}

/// Evaluate a Tera expression as a truthy/falsy boolean. Accepts either a
/// bare expression (`yui.os == 'linux'`) or a pre-wrapped one
/// (`{{ yui.os == 'linux' }}`); used for both file-header `yui:when` and
/// config `[[render.rule]] when` to keep the user-facing forms consistent
/// with `[[mount.entry]] when` (which the user writes wrapped).
fn eval_when(expr: &str, engine: &mut Engine, ctx: &TeraContext) -> Result<bool> {
    let trimmed = expr.trim_start();
    let to_render = if trimmed.starts_with("{{") || trimmed.starts_with("{%") {
        expr.to_string()
    } else {
        format!("{{{{ {expr} }}}}")
    };
    let out = engine.render(&to_render, ctx)?;
    let s = out.trim();
    Ok(s.eq_ignore_ascii_case("true") || s == "1")
}

fn template_target(template_path: &Utf8Path) -> Utf8PathBuf {
    let s = template_path.as_str();
    debug_assert!(s.ends_with(".tera"));
    Utf8PathBuf::from(&s[..s.len() - ".tera".len()])
}

fn relative_to(base: &Utf8Path, p: &Utf8Path) -> Utf8PathBuf {
    p.strip_prefix(base)
        .map(Utf8PathBuf::from)
        .unwrap_or_else(|_| p.to_path_buf())
}

fn collect_managed_paths(report: &RenderReport) -> Vec<Utf8PathBuf> {
    let mut all: Vec<_> = report
        .written
        .iter()
        .chain(report.unchanged.iter())
        .chain(report.diverged.iter())
        .cloned()
        .collect();
    all.sort();
    all.dedup();
    all
}

fn update_gitignore(source: &Utf8Path, rendered_abs_paths: &[Utf8PathBuf]) -> Result<()> {
    let gi_path = source.join(".gitignore");
    let existing = match std::fs::read_to_string(&gi_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Template(format!("read {gi_path}: {e}"))),
    };

    let mut managed: Vec<String> = rendered_abs_paths
        .iter()
        .filter_map(|p| p.strip_prefix(source).ok())
        .map(|p| p.as_str().replace('\\', "/"))
        .collect();
    managed.sort();
    managed.dedup();

    let new_section = build_managed_section(&managed);
    let updated = replace_or_append_section(&existing, &new_section);

    if updated != existing {
        std::fs::write(&gi_path, updated)?;
    }
    Ok(())
}

fn build_managed_section(lines: &[String]) -> String {
    let mut s = String::new();
    s.push_str(GITIGNORE_BEGIN);
    s.push('\n');
    for l in lines {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str(GITIGNORE_END);
    s.push('\n');
    s
}

fn replace_or_append_section(existing: &str, new_section: &str) -> String {
    // Refactored from `if let ... && cond` (let-chains) to nested if so the
    // crate's MSRV (rust-version = "1.85") stays buildable; let-chains were
    // stabilized in 1.88.
    if let (Some(start), Some(end)) = (existing.find(GITIGNORE_BEGIN), existing.find(GITIGNORE_END))
    {
        if start < end {
            let end_line_end = match existing[end..].find('\n') {
                Some(idx) => end + idx + 1,
                None => existing.len(),
            };
            let mut out = String::with_capacity(existing.len() + new_section.len());
            out.push_str(&existing[..start]);
            out.push_str(new_section);
            out.push_str(&existing[end_line_end..]);
            return out;
        }
    }

    let mut out = String::from(existing);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(new_section);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn yui_vars(source: &Utf8Path) -> YuiVars {
        YuiVars {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: "test".into(),
            user: "u".into(),
            source: source.to_string(),
        }
    }

    fn root(tmp: &TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap()
    }

    fn empty_config() -> Config {
        Config::default()
    }

    fn write(p: &Utf8Path, body: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn split_yui_when_basic() {
        assert_eq!(
            split_yui_when("{# yui:when yui.os == 'linux' #}\nbody"),
            Some(("yui.os == 'linux'", "body"))
        );
        assert_eq!(
            split_yui_when("\n  {#yui:when 1 == 1#}\nbody"),
            Some(("1 == 1", "body"))
        );
        // CRLF line endings
        assert_eq!(
            split_yui_when("{# yui:when true #}\r\nbody"),
            Some(("true", "body"))
        );
        // No newline after header — header still parsed, body is the rest
        assert_eq!(
            split_yui_when("{# yui:when true #}body"),
            Some(("true", "body"))
        );
        assert_eq!(split_yui_when("body without header"), None);
        assert_eq!(split_yui_when("{# regular comment #}body"), None);
    }

    #[test]
    fn template_target_strips_tera_extension() {
        assert_eq!(
            template_target(Utf8Path::new("/a/b/foo.tera")),
            Utf8PathBuf::from("/a/b/foo")
        );
        assert_eq!(
            template_target(Utf8Path::new("home/.gitconfig.tera")),
            Utf8PathBuf::from("home/.gitconfig")
        );
    }

    #[test]
    fn renders_simple_template_to_sibling() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(
            &r.join("home/.gitconfig.tera"),
            "[user]\n  os = {{ yui.os }}\n",
        );
        let report = render_all(&r, &empty_config(), &yui_vars(&r), false).unwrap();
        assert_eq!(report.written.len(), 1);
        assert_eq!(
            std::fs::read_to_string(r.join("home/.gitconfig")).unwrap(),
            "[user]\n  os = linux\n"
        );
    }

    #[test]
    fn renders_user_vars() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join("home/foo.tera"), "{{ vars.greet }}");
        let mut cfg = empty_config();
        cfg.vars
            .insert("greet".into(), toml::Value::String("hello".into()));
        let _ = render_all(&r, &cfg, &yui_vars(&r), false).unwrap();
        assert_eq!(
            std::fs::read_to_string(r.join("home/foo")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn skips_when_file_header_false() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(
            &r.join("home/foo.tera"),
            "{# yui:when yui.os == 'windows' #}\nbody",
        );
        let report = render_all(&r, &empty_config(), &yui_vars(&r), false).unwrap();
        assert!(report.written.is_empty());
        assert_eq!(report.skipped_when_false.len(), 1);
        assert!(!r.join("home/foo").exists());
    }

    #[test]
    fn includes_when_file_header_true() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(
            &r.join("home/foo.tera"),
            "{# yui:when yui.os == 'linux' #}\nbody",
        );
        let report = render_all(&r, &empty_config(), &yui_vars(&r), false).unwrap();
        assert_eq!(report.written.len(), 1);
        assert_eq!(std::fs::read_to_string(r.join("home/foo")).unwrap(), "body");
    }

    #[test]
    fn config_rule_when_false_skips_matching_template() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join("home/win/settings.tera"), "body");
        let mut cfg = empty_config();
        cfg.render.rule.push(RenderRule {
            r#match: "home/win/**".into(),
            when: Some("{{ yui.os == 'windows' }}".into()),
        });
        let report = render_all(&r, &cfg, &yui_vars(&r), false).unwrap();
        assert_eq!(report.skipped_when_false.len(), 1);
        assert!(report.written.is_empty());
    }

    #[test]
    fn config_rule_no_match_does_not_filter() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join("home/foo.tera"), "body");
        let mut cfg = empty_config();
        // glob doesn't match foo.tera
        cfg.render.rule.push(RenderRule {
            r#match: "home/win/**".into(),
            when: Some("{{ yui.os == 'windows' }}".into()),
        });
        let report = render_all(&r, &cfg, &yui_vars(&r), false).unwrap();
        assert_eq!(report.written.len(), 1);
    }

    #[test]
    fn unchanged_when_existing_matches() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join("home/foo.tera"), "body");
        write(&r.join("home/foo"), "body"); // already in sync
        let report = render_all(&r, &empty_config(), &yui_vars(&r), false).unwrap();
        assert!(report.written.is_empty());
        assert_eq!(report.unchanged.len(), 1);
    }

    #[test]
    fn detects_drift_when_existing_diverges() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join("home/foo.tera"), "fresh body");
        write(&r.join("home/foo"), "manually edited");
        let report = render_all(&r, &empty_config(), &yui_vars(&r), false).unwrap();
        assert!(report.has_drift());
        assert_eq!(report.diverged.len(), 1);
        // existing content NOT overwritten
        assert_eq!(
            std::fs::read_to_string(r.join("home/foo")).unwrap(),
            "manually edited"
        );
    }

    #[test]
    fn dry_run_does_not_write_or_touch_gitignore() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join("home/foo.tera"), "body");
        let _ = render_all(&r, &empty_config(), &yui_vars(&r), true).unwrap();
        assert!(!r.join("home/foo").exists());
        assert!(!r.join(".gitignore").exists());
    }

    #[test]
    fn updates_gitignore_managed_section() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join("home/foo.tera"), "body");
        write(&r.join("home/bar.tera"), "body2");
        let _ = render_all(&r, &empty_config(), &yui_vars(&r), false).unwrap();
        let gi = std::fs::read_to_string(r.join(".gitignore")).unwrap();
        assert!(gi.contains(GITIGNORE_BEGIN));
        assert!(gi.contains(GITIGNORE_END));
        assert!(gi.contains("home/bar"));
        assert!(gi.contains("home/foo"));
        // sorted: bar before foo
        let bar_pos = gi.find("home/bar").unwrap();
        let foo_pos = gi.find("home/foo").unwrap();
        assert!(bar_pos < foo_pos);
    }

    #[test]
    fn preserves_existing_gitignore_content() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join(".gitignore"), "node_modules/\ntarget/\n");
        write(&r.join("home/foo.tera"), "body");
        let _ = render_all(&r, &empty_config(), &yui_vars(&r), false).unwrap();
        let gi = std::fs::read_to_string(r.join(".gitignore")).unwrap();
        assert!(gi.contains("node_modules/"));
        assert!(gi.contains("target/"));
        assert!(gi.contains("home/foo"));
    }

    #[test]
    fn replaces_existing_managed_section() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        // Pre-existing managed section with stale content
        write(
            &r.join(".gitignore"),
            &format!("node_modules/\n\n{GITIGNORE_BEGIN}\nstale/path\n{GITIGNORE_END}\n\nfoo\n"),
        );
        write(&r.join("home/foo.tera"), "body");
        let _ = render_all(&r, &empty_config(), &yui_vars(&r), false).unwrap();
        let gi = std::fs::read_to_string(r.join(".gitignore")).unwrap();
        assert!(gi.contains("node_modules/"));
        assert!(gi.contains("home/foo"));
        assert!(!gi.contains("stale/path"));
        // post-section content preserved
        assert!(gi.contains("\nfoo\n"));
    }

    #[test]
    fn walks_into_gitignored_directories() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        // Pre-existing .gitignore that would normally hide `node_modules/`.
        write(&r.join(".gitignore"), "node_modules/\n");
        write(&r.join("node_modules/foo.tera"), "body");
        let report = render_all(&r, &empty_config(), &yui_vars(&r), false).unwrap();
        // Template under a gitignored dir is still discovered + rendered.
        assert_eq!(report.written.len(), 1);
        assert!(r.join("node_modules/foo").exists());
    }

    #[test]
    fn removes_stale_rendered_when_file_header_becomes_false() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        // Previously rendered output sitting on disk
        write(
            &r.join("home/foo.tera"),
            "{# yui:when yui.os == 'windows' #}\nbody",
        );
        write(&r.join("home/foo"), "old rendered output");
        let report = render_all(&r, &empty_config(), &yui_vars(&r), false).unwrap();
        assert_eq!(report.skipped_when_false.len(), 1);
        // Stale sibling was cleaned up so apply won't link it.
        assert!(!r.join("home/foo").exists());
    }

    #[test]
    fn removes_stale_rendered_when_rule_when_becomes_false() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join("home/win/settings.tera"), "body");
        write(&r.join("home/win/settings"), "old rendered output");
        let mut cfg = empty_config();
        cfg.render.rule.push(RenderRule {
            r#match: "home/win/**".into(),
            when: Some("{{ yui.os == 'windows' }}".into()),
        });
        let report = render_all(&r, &cfg, &yui_vars(&r), false).unwrap();
        assert_eq!(report.skipped_when_false.len(), 1);
        assert!(!r.join("home/win/settings").exists());
    }

    #[test]
    fn dry_run_does_not_remove_stale_rendered() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join("home/foo.tera"), "{# yui:when false #}\nbody");
        write(&r.join("home/foo"), "old rendered output");
        let _ = render_all(&r, &empty_config(), &yui_vars(&r), true).unwrap();
        // Dry-run leaves the on-disk file alone.
        assert_eq!(
            std::fs::read_to_string(r.join("home/foo")).unwrap(),
            "old rendered output"
        );
    }

    #[test]
    fn manage_gitignore_disabled_does_not_write_gitignore() {
        let tmp = TempDir::new().unwrap();
        let r = root(&tmp);
        write(&r.join("home/foo.tera"), "body");
        let mut cfg = empty_config();
        cfg.render.manage_gitignore = false;
        let _ = render_all(&r, &cfg, &yui_vars(&r), false).unwrap();
        assert!(r.join("home/foo").exists());
        assert!(!r.join(".gitignore").exists());
    }
}
