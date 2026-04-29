//! Command implementations.
//!
//! Each `Command` variant in `cli.rs` calls one of these. Currently
//! implemented: `apply`, `init`, `doctor`. The rest are `todo!()`.

use anyhow::{Context as _, Result};
use camino::{Utf8Path, Utf8PathBuf};
use tracing::{info, warn};

use crate::config::{self, Config, MountStrategy};
use crate::link::{self, EffectiveDirMode, EffectiveFileMode, resolve_dir_mode, resolve_file_mode};
use crate::marker;
use crate::mount::{self, ResolvedMount};
use crate::template;
use crate::vars::YuiVars;
use crate::{backup, paths};

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

    let mut engine = template::Engine::new();
    let ctx = template::config_context(&yui);
    let mounts = mount::resolve(
        &config.mount.entry,
        config.mount.default_strategy,
        &mut engine,
        &ctx,
    )?;

    let backup_root = source.join(&config.backup.dir);
    let ctx = ApplyCtx {
        config: &config,
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
        process_mount(&source, m, &ctx)?;
    }
    Ok(())
}

/// Bundle of immutable settings threaded through the apply walk.
struct ApplyCtx<'a> {
    config: &'a Config,
    file_mode: EffectiveFileMode,
    dir_mode: EffectiveDirMode,
    backup_root: &'a Utf8Path,
    dry_run: bool,
}

pub fn render(_source: Option<Utf8PathBuf>, _check: bool, _dry_run: bool) -> Result<()> {
    todo!("yui render — Tera rendering of *.tera files (next iteration)")
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

pub fn status(_source: Option<Utf8PathBuf>) -> Result<()> {
    todo!("yui status — drift detection (needs absorb classifier)")
}

pub fn absorb(_source: Option<Utf8PathBuf>, _target: Utf8PathBuf, _dry_run: bool) -> Result<()> {
    todo!("yui absorb — manual absorb (needs absorb classifier)")
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

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

fn process_mount(source: &Utf8Path, m: &ResolvedMount, ctx: &ApplyCtx<'_>) -> Result<()> {
    let src_root = source.join(&m.src);
    if !src_root.is_dir() {
        warn!("mount src missing: {src_root}");
        return Ok(());
    }
    walk_and_link(&src_root, &m.dst, ctx, m.strategy)
}

fn walk_and_link(
    src_dir: &Utf8Path,
    dst_dir: &Utf8Path,
    ctx: &ApplyCtx<'_>,
    strategy: MountStrategy,
) -> Result<()> {
    let marker_filename = &ctx.config.mount.marker_filename;

    if strategy == MountStrategy::Marker && marker::is_marker_dir(src_dir, marker_filename) {
        link_dir_with_backup(src_dir, dst_dir, ctx)?;
        return Ok(());
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
            // Templates handled by render flow (not implemented yet).
            continue;
        }

        let src_path = src_dir.join(name);
        let dst_path = dst_dir.join(name);
        let ft = entry.file_type()?;

        if ft.is_dir() {
            walk_and_link(&src_path, &dst_path, ctx, strategy)?;
        } else if ft.is_file() {
            link_file_with_backup(&src_path, &dst_path, ctx)?;
        }
    }
    Ok(())
}

fn link_file_with_backup(src: &Utf8Path, dst: &Utf8Path, ctx: &ApplyCtx<'_>) -> Result<()> {
    if ctx.dry_run {
        info!("[dry-run] link file: {src} → {dst}");
        return Ok(());
    }
    if std::fs::symlink_metadata(dst).is_ok() {
        backup_existing(dst, ctx.backup_root, /*is_dir=*/ false)?;
        link::unlink(dst)?;
    }
    info!("link file: {src} → {dst}");
    link::link_file(src, dst, ctx.file_mode)?;
    Ok(())
}

fn link_dir_with_backup(src: &Utf8Path, dst: &Utf8Path, ctx: &ApplyCtx<'_>) -> Result<()> {
    if ctx.dry_run {
        info!("[dry-run] link dir: {src} → {dst}");
        return Ok(());
    }
    if std::fs::symlink_metadata(dst).is_ok() {
        backup_existing(dst, ctx.backup_root, /*is_dir=*/ true)?;
        link::unlink(dst)?;
    }
    info!("link dir: {src} → {dst}");
    link::link_dir(src, dst, ctx.dir_mode)?;
    Ok(())
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
    if let Some(home) = home_dir() {
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
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let cwd = current_dir_utf8()?;
    Ok(cwd.join(p))
}

fn current_dir_utf8() -> Result<Utf8PathBuf> {
    let cwd = std::env::current_dir().context("getting cwd")?;
    Utf8PathBuf::from_path_buf(cwd).map_err(|p| anyhow::anyhow!("non-UTF8 cwd: {}", p.display()))
}

fn home_dir() -> Option<Utf8PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(Utf8PathBuf::from)
}

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
dst = "{{ env(name='HOME') | default(value=env(name='USERPROFILE')) }}"

# [[mount.entry]]
# src  = "appdata"
# dst  = "{{ env(name='APPDATA') }}"
# when = "{{ yui.os == 'windows' }}"
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
    fn apply_skips_tera_files() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("home/.gitconfig.tera"), "stuff").unwrap();
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

        apply(Some(source), false).unwrap();

        assert!(target.join(".bashrc").exists());
        // Templates are NOT linked yet (pending render flow).
        assert!(!target.join(".gitconfig").exists());
        assert!(!target.join(".gitconfig.tera").exists());
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

    #[test]
    fn apply_with_existing_target_backs_up() {
        let tmp = TempDir::new().unwrap();
        let source = utf8(tmp.path().join("dotfiles"));
        let target = utf8(tmp.path().join("target"));
        std::fs::create_dir_all(source.join("home")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("home/.bashrc"), "new content").unwrap();
        // Pre-existing target file with different content.
        std::fs::write(target.join(".bashrc"), "old content").unwrap();

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

        // Target now has new content (linked from source).
        assert_eq!(
            std::fs::read_to_string(target.join(".bashrc")).unwrap(),
            "new content"
        );

        // A backup of the old content should exist somewhere under .yui/backup.
        let backup_root = source.join(".yui/backup");
        assert!(backup_root.exists(), "backup root should exist");
        let mut found_old = false;
        for entry in walkdir(&backup_root) {
            if let Ok(s) = std::fs::read_to_string(&entry) {
                if s == "old content" {
                    found_old = true;
                    break;
                }
            }
        }
        assert!(found_old, "expected backup containing 'old content'");
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
