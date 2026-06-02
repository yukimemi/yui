use super::*;
use crate::config::{self, Config};
use crate::link::{resolve_dir_mode, resolve_file_mode};
use crate::render;
use crate::secret;
use crate::vars::YuiVars;
use crate::{absorb, paths};
use camino::{Utf8Path, Utf8PathBuf};
use std::cell::Cell;
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
fn apply_skips_render_drift_off_tty() {
    // Render drift used to abort apply with a `bail!`. New behaviour
    // is to resolve interactively — and off-TTY (where `cargo test`
    // runs) the prompt defaults to Skip, so apply proceeds. The
    // rendered file is left alone (no clobber) and the link pass
    // still wires up the target from what's on disk.
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

    apply(Some(source.clone()), false).unwrap();
    // Off-TTY prompt default = Skip → rendered file untouched.
    assert_eq!(
        std::fs::read_to_string(source.join("home/foo")).unwrap(),
        "manually edited"
    );
    // Link pass still wires up the target from the (skipped)
    // rendered file.
    assert_eq!(
        std::fs::read_to_string(target.join("foo")).unwrap(),
        "manually edited"
    );
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
    let result = append_recipient_to_config("", "host alice", "age1abcrecipientpublickey").unwrap();
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
    let rendered = render::render_to_string(&source.join("home/note.tera"), &source, &cfg, &yui)
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
        msg.contains("not a git repository") || msg.contains("uncommitted") || msg.contains("git"),
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
fn prompt_anomaly_short_circuits_on_quit_requested() {
    // Once `[q]uit` flips the cell, every subsequent prompt
    // (including the cascade of per-file prompts inside an
    // in-flight dir merge) returns `Quit` immediately so we don't
    // re-prompt or block on stdin during teardown.
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
        sticky_anomaly: Cell::new(None),
        quit_requested: Cell::new(true),
    };

    let got = prompt_anomaly(&ctx, &src_file, &dst_file, "test").unwrap();
    assert_eq!(got, AnomalyChoice::Quit);
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
