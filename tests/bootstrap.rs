use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};

fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    target: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let repo = temp.path().join("remote");
        let target = temp.path().join("target");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init"]);
        std::fs::write(
            repo.join("config.toml"),
            format!(
                "[[mount.entry]]\nsrc = 'files'\ndst = '{}'\n",
                target.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        std::fs::create_dir(repo.join("files")).unwrap();
        std::fs::write(repo.join("files/config"), "original").unwrap();
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "fixture",
            ],
        );
        Self {
            _temp: temp,
            home,
            repo,
            target,
        }
    }

    fn url(&self) -> String {
        format!("file://{}", self.repo.to_string_lossy().replace('\\', "/"))
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("yui"));
        cmd.current_dir(&self.home)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.join("absent-gitconfig"))
            .env_remove("YUI_SOURCE")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE");
        cmd
    }

    fn destination(&self) -> PathBuf {
        self.home.join("dotfiles")
    }
}

#[test]
fn clone_only_then_apply_and_preserve_dirty_reuse() {
    let f = Fixture::new();
    f.cmd().args(["init", &f.url()]).assert().success();
    assert!(f.destination().join("config.toml").is_file());
    assert!(!f.destination().join(".gitignore").exists());
    assert!(!f.target.exists());
    std::fs::write(f.destination().join("files/config"), "edited").unwrap();
    std::fs::write(f.destination().join("untracked"), "keep").unwrap();
    std::fs::write(f.repo.join("upstream-only"), "new remote commit").unwrap();
    git(&f.repo, &["add", "."]);
    git(
        &f.repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "advance remote",
        ],
    );
    f.cmd()
        .args(["init", &f.url(), "--apply"])
        .assert()
        .success();
    assert!(!f.destination().join("upstream-only").exists());
    assert_eq!(
        std::fs::read_to_string(f.destination().join("untracked")).unwrap(),
        "keep"
    );
    assert_eq!(
        std::fs::read_to_string(f.target.join("config")).unwrap(),
        "edited"
    );
    assert_eq!(
        std::fs::read_to_string(f.destination().join("files/config")).unwrap(),
        "edited"
    );
}

#[test]
fn explicit_destination_and_env_source() {
    let f = Fixture::new();
    f.cmd()
        .args(["init", &f.url(), "--source", "custom", "--apply"])
        .assert()
        .success();
    assert!(f.home.join("custom/config.toml").is_file());
    assert!(!f.destination().exists());
    assert!(f.target.join("config").exists());
    f.cmd()
        .env("YUI_SOURCE", "from-env")
        .args(["init", &f.url()])
        .assert()
        .success();
    assert!(f.home.join("from-env/config.toml").is_file());
}

#[test]
fn clone_failure_never_applies_or_publishes() {
    let f = Fixture::new();
    f.cmd()
        .args([
            "init",
            &format!("{}/missing", f.url()),
            "--apply",
            "--git-hooks",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("git clone failed"));
    assert!(!f.destination().exists());
    assert!(!f.target.exists());
    assert!(!std::fs::read_dir(&f.home).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".yui-clone-")
    }));
}

#[test]
fn existing_destinations_are_preserved() {
    for kind in ["empty", "file", "unrelated", "no-origin", "bare", "nested"] {
        let f = Fixture::new();
        let dest = f.destination();
        match kind {
            "file" => std::fs::write(&dest, "keep").unwrap(),
            "bare" => {
                std::fs::create_dir(&dest).unwrap();
                git(&dest, &["init", "--bare"]);
            }
            "nested" => {
                git(&f.home, &["init"]);
                std::fs::create_dir(&dest).unwrap();
            }
            "no-origin" => {
                f.cmd().args(["init", &f.url()]).assert().success();
                git(&dest, &["remote", "remove", "origin"]);
            }
            "unrelated" => {
                f.cmd().args(["init", &f.url()]).assert().success();
                git(
                    &dest,
                    &[
                        "remote",
                        "set-url",
                        "origin",
                        "https://example.invalid/other.git",
                    ],
                );
            }
            _ => std::fs::create_dir(&dest).unwrap(),
        }
        f.cmd()
            .args(["init", &f.url(), "--apply"])
            .assert()
            .failure();
        assert!(dest.exists());
        assert!(!f.target.exists());
        if kind == "file" {
            assert_eq!(std::fs::read_to_string(dest).unwrap(), "keep");
        }
    }
}

#[cfg(unix)]
#[test]
fn symlink_destination_is_refused_including_dangling() {
    let f = Fixture::new();
    std::os::unix::fs::symlink(&f.repo, f.destination()).unwrap();
    f.cmd().args(["init", &f.url()]).assert().failure();
    assert!(
        std::fs::symlink_metadata(f.destination())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    std::fs::remove_file(f.destination()).unwrap();
    std::os::unix::fs::symlink(f.home.join("missing"), f.destination()).unwrap();
    f.cmd().args(["init", &f.url()]).assert().failure();
    assert!(
        std::fs::symlink_metadata(f.destination())
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn hooks_are_installed_and_existing_hooks_preserved() {
    let f = Fixture::new();
    f.cmd()
        .args(["init", &f.url(), "--git-hooks"])
        .assert()
        .success();
    let hook = f.destination().join(".git/hooks/pre-commit");
    assert!(
        std::fs::read_to_string(&hook)
            .unwrap()
            .contains("yui render --check")
    );
    std::fs::write(&hook, "custom hook").unwrap();
    f.cmd()
        .args(["init", &f.url(), "--git-hooks"])
        .assert()
        .success();
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), "custom hook");
    assert!(!f.destination().join(".gitignore").exists());
}

#[test]
fn missing_config_warns_and_apply_fails_without_scaffolding() {
    let f = Fixture::new();
    std::fs::remove_file(f.repo.join("config.toml")).unwrap();
    git(&f.repo, &["add", "-u"]);
    git(
        &f.repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "remove config",
        ],
    );
    f.cmd()
        .args(["init", &f.url()])
        .assert()
        .success()
        .stdout(predicate::str::contains("config.toml is missing"));
    f.cmd()
        .args(["init", &f.url(), "--apply"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("config.toml is missing"));
    assert!(f.destination().join(".git").exists());
    assert!(!f.destination().join("config.toml").exists());
    assert!(!f.target.exists());
}

#[test]
fn skeleton_and_apply_argument_requirement() {
    let f = Fixture::new();
    f.cmd()
        .args(["init", "--apply"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("<REPO>"));
    assert!(!f.home.join("config.toml").exists());
    f.cmd().arg("init").assert().success();
    assert!(f.home.join("config.toml").is_file());
    assert!(f.home.join(".gitignore").is_file());
    assert!(!f.destination().exists());
}

#[test]
fn apply_failure_keeps_initialized_repository() {
    let f = Fixture::new();
    std::fs::write(f.repo.join("config.toml"), "invalid = [").unwrap();
    git(&f.repo, &["add", "."]);
    git(
        &f.repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "invalid config",
        ],
    );
    f.cmd()
        .args(["init", &f.url(), "--apply"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("repository remains initialized"));
    assert!(f.destination().join(".git").exists());
    assert_eq!(
        std::fs::read_to_string(f.destination().join("config.toml")).unwrap(),
        "invalid = ["
    );
    assert!(!f.target.exists());
}

#[cfg(unix)]
#[test]
fn publication_conflict_preserves_destination_and_retrieved_clone() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let template = f.home.join("git-template");
    std::fs::create_dir_all(template.join("hooks")).unwrap();
    let hook = template.join("hooks/post-checkout");
    std::fs::write(&hook, "#!/bin/sh\nmkdir \"$BOOTSTRAP_CONFLICT_DEST\"\nprintf keep > \"$BOOTSTRAP_CONFLICT_DEST/keep\"\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    f.cmd()
        .env("GIT_TEMPLATE_DIR", &template)
        .env("BOOTSTRAP_CONFLICT_DEST", f.destination())
        .args(["init", &f.url(), "--apply"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Retrieved repository is preserved",
        ));
    assert_eq!(
        std::fs::read_to_string(f.destination().join("keep")).unwrap(),
        "keep"
    );
    assert!(!f.destination().join(".git").exists());
    let staged = std::fs::read_dir(&f.home)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".yui-clone-")
        })
        .unwrap();
    assert!(staged.join("config.toml").is_file());
    assert!(!f.target.exists());
}

#[cfg(unix)]
#[test]
fn clone_failure_does_not_run_apply_hooks_from_current_source() {
    let f = Fixture::new();
    let marker = f.home.join("hook-ran");
    std::fs::write(
        f.home.join("config.toml"),
        "[[hook]]\nname = 'sentinel'\nscript = 'sentinel.sh'\nphase = 'pre'\nwhen_run = 'every'\n",
    )
    .unwrap();
    std::fs::write(f.home.join("sentinel.sh"), "printf ran > hook-ran\n").unwrap();
    f.cmd()
        .args(["init", &format!("{}/missing", f.url()), "--apply"])
        .assert()
        .failure();
    assert!(!marker.exists());
    assert!(!f.home.join(".yui/state.json").exists());
    // Prove the sentinel would run if initialization accidentally applied cwd.
    f.cmd().arg("apply").assert().success();
    assert!(marker.exists());
}
