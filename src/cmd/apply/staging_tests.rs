use super::*;
use tempfile::TempDir;

fn with_ctx(test: impl FnOnce(&Utf8Path, &ApplyCtx<'_>)) {
    let tmp = TempDir::new().unwrap();
    let root = Utf8Path::from_path(tmp.path()).unwrap();
    let config = Config::default();
    let plan = LinkPlan::default();
    let backup_root = root.join("backup");
    let ctx = ApplyCtx {
        config: &config,
        plan: &plan,
        file_mode: resolve_file_mode(config.mount.file_mode),
        dir_mode: resolve_dir_mode(config.mount.dir_mode),
        backup_root: &backup_root,
        dry_run: false,
        sticky_anomaly: Cell::new(None),
        quit_requested: Cell::new(false),
        source_clean: true,
        unresolved: RefCell::new(Vec::new()),
    };
    test(root, &ctx);
}

#[cfg(windows)]
fn check_locked_target(overwrite: bool) {
    use std::os::windows::fs::OpenOptionsExt;

    with_ctx(|root, ctx| {
        let src = root.join("source");
        let dst = root.join("target");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(dst.join("nested")).unwrap();
        std::fs::write(src.join("source-only"), "source").unwrap();
        std::fs::write(dst.join("config.toml"), "original settings").unwrap();
        std::fs::write(dst.join("nested/keep"), "original data").unwrap();

        // A directory handle without FILE_SHARE_DELETE denies staging while
        // leaving its children deletable: the old fallback destroyed them.
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2) // FILE_SHARE_READ | FILE_SHARE_WRITE
            .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
            .open(&dst)
            .unwrap();
        let result = if overwrite {
            overwrite_source_dir_into_target(&src, &dst, ctx, ctx.dir_mode)
        } else {
            absorb_target_dir_into_source(&src, &dst, ctx, ctx.dir_mode)
        };
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("target left unchanged"), "{error}");
        assert!(error.contains("Close applications"), "{error}");
        assert!(
            !std::fs::symlink_metadata(&dst)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("config.toml")).unwrap(),
            "original settings"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("nested/keep")).unwrap(),
            "original data"
        );
        assert_eq!(
            std::fs::read_to_string(src.join("source-only")).unwrap(),
            "source"
        );
        assert!(
            !src.join("config.toml").exists(),
            "failed staging must not merge"
        );
        assert!(
            !src.join("nested").exists(),
            "failed staging must not merge"
        );
        assert!(paths::scan_staged(&dst).is_empty());

        drop(handle);
        if overwrite {
            overwrite_source_dir_into_target(&src, &dst, ctx, ctx.dir_mode).unwrap();
        } else {
            absorb_target_dir_into_source(&src, &dst, ctx, ctx.dir_mode).unwrap();
            assert_eq!(
                std::fs::read_to_string(dst.join("nested/keep")).unwrap(),
                "original data"
            );
        }
        assert_eq!(
            absorb::classify(&src, &dst).unwrap(),
            absorb::AbsorbDecision::InSync
        );
        assert!(paths::scan_staged(&dst).is_empty());
    });
}

#[test]
#[cfg(windows)]
fn absorb_preserves_locked_target_without_merging() {
    check_locked_target(false);
}

#[test]
#[cfg(windows)]
fn overwrite_preserves_locked_target() {
    check_locked_target(true);
}

fn check_failed_link(overwrite: bool) {
    with_ctx(|root, ctx| {
        let dst = root.join("target");
        std::fs::create_dir_all(dst.join("nested")).unwrap();
        std::fs::write(dst.join("nested/keep"), "original data").unwrap();
        // NUL makes link creation fail on every OS without needing privileges
        // or changing global process state. The target can still be staged.
        let src = root.join("invalid\0source");
        let result = if overwrite {
            overwrite_source_dir_into_target(&src, &dst, ctx, EffectiveDirMode::Symlink)
        } else {
            absorb_target_dir_into_source(&src, &dst, ctx, EffectiveDirMode::Symlink)
        };
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("original target restored"), "{error}");
        assert!(
            !std::fs::symlink_metadata(&dst)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("nested/keep")).unwrap(),
            "original data"
        );
        assert!(paths::scan_staged(&dst).is_empty());
    });
}

#[test]
fn absorb_restores_target_when_link_creation_fails() {
    check_failed_link(false);
}

#[test]
fn overwrite_restores_target_when_link_creation_fails() {
    check_failed_link(true);
}

#[test]
fn failed_link_retains_staging_when_destination_is_occupied() {
    with_ctx(|root, ctx| {
        let src = root.join("source");
        let dst = root.join("target");
        let staged =
            paths::staged_path(&dst, paths::StagedKind::Absorb, "20260101_000000000").unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("keep"), "original data").unwrap();
        std::fs::write(&dst, "new occupant").unwrap();

        let error = format!(
            "{:#}",
            link_staged_dir(&src, &dst, &staged, ctx.dir_mode).unwrap_err()
        );
        assert!(error.contains("original target retained at"), "{error}");
        assert!(error.contains(staged.as_str()), "{error}");
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "new occupant");
        assert_eq!(
            std::fs::read_to_string(staged.join("keep")).unwrap(),
            "original data"
        );
    });
}

#[test]
fn rollback_never_replaces_an_existing_empty_directory() {
    with_ctx(|root, _ctx| {
        let staged = root.join("staged");
        let dst = root.join("target");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(staged.join("keep"), "original data").unwrap();
        let before = same_file::Handle::from_path(&dst).unwrap();

        assert!(link::rename_dir_noreplace(&staged, &dst).is_err());
        assert_eq!(before, same_file::Handle::from_path(&dst).unwrap());
        assert_eq!(std::fs::read_dir(&dst).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_to_string(staged.join("keep")).unwrap(),
            "original data"
        );
        drop(before);

        std::fs::remove_dir(&dst).unwrap();
        link::rename_dir_noreplace(&staged, &dst).unwrap();
        assert!(!staged.exists());
        assert_eq!(
            std::fs::read_to_string(dst.join("keep")).unwrap(),
            "original data"
        );
    });
}

#[test]
fn retry_preserves_blocked_recovery_until_the_link_can_be_installed() {
    for kind in [paths::StagedKind::Absorb, paths::StagedKind::Discard] {
        with_ctx(|root, ctx| {
            let src = root.join("source");
            let dst = root.join("target");
            let staged = paths::staged_path(&dst, kind, "20260101_000000000").unwrap();
            std::fs::create_dir_all(&src).unwrap();
            std::fs::create_dir_all(&staged).unwrap();
            std::fs::write(src.join("source-only"), "source").unwrap();
            std::fs::write(staged.join("keep"), "original data").unwrap();
            std::fs::write(&dst, "new occupant").unwrap();
            assert!(link_staged_dir(&src, &dst, &staged, ctx.dir_mode).is_err());

            let error = format!(
                "{:#}",
                link_dir_with_backup(&src, &dst, ctx, ctx.dir_mode).unwrap_err()
            );
            assert!(error.contains("does not link to"), "{error}");
            assert_eq!(std::fs::read_to_string(&dst).unwrap(), "new occupant");
            assert_eq!(
                std::fs::read_to_string(staged.join("keep")).unwrap(),
                "original data"
            );
            assert_eq!(
                std::fs::read_to_string(src.join("source-only")).unwrap(),
                "source"
            );
            assert!(!src.join("keep").exists());

            std::fs::remove_file(&dst).unwrap();
            link_dir_with_backup(&src, &dst, ctx, ctx.dir_mode).unwrap();
            assert!(same_file::is_same_file(&src, &dst).unwrap());
            assert!(!staged.exists());
            assert_eq!(src.join("keep").exists(), kind == paths::StagedKind::Absorb);
        });
    }
}
