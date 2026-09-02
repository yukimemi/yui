use super::*;
use crate::config::{self, IconsMode};
use crate::icons::Icons;
use crate::vars::YuiVars;
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

/// `yui gc-backup [--older-than DUR] [--keep-last N] [--dry-run]` —
/// prune snapshots under `$DOTFILES/.yui/backup/`.
///
/// With no rule we run a non-destructive *survey*: walk the backup
/// tree, list every entry whose name carries yui's
/// `_<YYYYMMDD_HHMMSSfff>[.<ext>]` suffix, and print AGE / SIZE / PATH
/// sorted oldest-first plus a hint about the flags that delete.
///
/// Two rules, and they compose as a *retention policy* rather than a
/// filter chain — **an entry survives if either rule protects it**:
///   - `--older-than DUR` (e.g. `30d`, `2w`, `12h`, `6mo`, `1y` —
///     note bare `m` is *minutes*, months need `mo`) keeps
///     anything newer than the cutoff.
///   - `--keep-last N` keeps the N newest snapshots of each target,
///     however old they are. `N = 0` protects nothing, which is the
///     honest spelling for "purge".
///
/// So `--older-than 30d --keep-last 3` prunes what's older than a
/// month while never dropping the three newest of anything. Age alone
/// can't help with the case that actually fills a disk: one fresh
/// snapshot of a large junctioned tree.
///
/// `--dry-run` previews whichever policy was given.
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
    keep_last: Option<usize>,
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

    if older_than.is_none() && keep_last.is_none() {
        let refs: Vec<&BackupEntry> = entries.iter().collect();
        print_gc_table(&refs, &backup_root, &now, icons, color);
        println!();
        println!(
            "  {} entries · {} total — pass --older-than DUR (e.g. 30d) \
             or --keep-last N to delete",
            entries.len(),
            format_bytes(entries.iter().map(|e| e.size_bytes).sum())
        );
        return Ok(());
    }

    let cutoff = match &older_than {
        Some(dur_str) => {
            let span = parse_human_duration(dur_str)?;
            Some(
                now.checked_sub(span)
                    .map_err(|e| anyhow::anyhow!("invalid duration {dur_str:?}: {e}"))?
                    .datetime(),
            )
        }
        None => None,
    };
    let protected = keep_last.map(|n| protected_by_generation(&entries, n));

    let total_before: u64 = entries.iter().map(|e| e.size_bytes).sum();
    let to_delete: Vec<&BackupEntry> = entries
        .iter()
        .enumerate()
        .filter(|(idx, entry)| {
            // Retention, not filtering: an entry survives if *any*
            // active rule protects it. With only one rule given, the
            // other simply has no opinion.
            let age_protects = cutoff.is_some_and(|c| entry.ts >= c);
            let generation_protects = protected.as_ref().is_some_and(|p| p.contains(idx));
            !(age_protects || generation_protects)
        })
        .map(|(_, entry)| entry)
        .collect();

    if to_delete.is_empty() {
        println!(
            "  nothing to prune under this policy ({}; oldest: {})",
            describe_policy(&older_than, keep_last),
            format_age(entries[0].ts, &now)
        );
        return Ok(());
    }

    print_gc_table(&to_delete, &backup_root, &now, icons, color);
    println!();
    let total_freed: u64 = to_delete.iter().map(|e| e.size_bytes).sum();

    if dry_run {
        println!(
            "  [dry-run] would remove {} of {} entries · would free {} of {} ({})",
            to_delete.len(),
            entries.len(),
            format_bytes(total_freed),
            format_bytes(total_before),
            describe_policy(&older_than, keep_last),
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

/// Indices of the `n` newest snapshots of every target.
///
/// The target is the entry's path with yui's timestamp suffix stripped,
/// so `home/.claude_<ts>/` and `home/.claude_<older ts>/` share a
/// bucket while two apps' `settings.json` don't (the parent path is
/// part of the key).
fn protected_by_generation(entries: &[BackupEntry], n: usize) -> std::collections::HashSet<usize> {
    use std::collections::HashMap;
    let mut by_target: HashMap<Utf8PathBuf, Vec<usize>> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        let Some(name) = entry.path.file_name() else {
            continue;
        };
        let Some(target) = backup_target_name(name) else {
            continue;
        };
        let key = entry
            .path
            .parent()
            .map(|p| p.join(&target))
            .unwrap_or_else(|| Utf8PathBuf::from(target));
        by_target.entry(key).or_default().push(idx);
    }
    let mut protected = std::collections::HashSet::new();
    for idxs in by_target.values() {
        // `entries` is sorted oldest-first, so the tail is the newest.
        for idx in idxs.iter().rev().take(n) {
            protected.insert(*idx);
        }
    }
    protected
}

fn describe_policy(older_than: &Option<String>, keep_last: Option<usize>) -> String {
    match (older_than, keep_last) {
        (Some(d), Some(n)) => format!("older than {d}, keeping the newest {n} per target"),
        (Some(d), None) => format!("older than {d}"),
        (None, Some(n)) => format!("keeping the newest {n} per target"),
        (None, None) => "survey".to_string(),
    }
}

#[derive(Debug)]
pub(crate) struct BackupEntry {
    pub(crate) path: Utf8PathBuf,
    pub(crate) ts: jiff::civil::DateTime,
    pub(crate) kind: BackupKind,
    pub(crate) size_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupKind {
    File,
    Dir,
}

/// Recursive walk that recognises directory backups as one unit
/// (so we don't descend into `<dirname>_<ts>/` and surface its
/// individual files — the whole subtree is one snapshot). Files
/// without a yui suffix are silently skipped.
pub(crate) fn walk_gc_backups(root: &Utf8Path) -> Result<Vec<BackupEntry>> {
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
pub(crate) fn cleanup_empty_parents(start: &Utf8Path, root: &Utf8Path) {
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
pub(crate) fn parse_backup_suffix(name: &str) -> Option<jiff::civil::DateTime> {
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

/// Strip yui's `_<YYYYMMDD_HHMMSSfff>` suffix back off, recovering the
/// name the snapshot was taken of: `config_20260815_020729950.yml` →
/// `config.yml`, `.claude_20260816_111625675` → `.claude`.
///
/// This is the inverse of [`crate::paths::append_timestamp`] and is
/// what groups snapshots of the same target together. Returns `None`
/// for anything that isn't yui-stamped, mirroring
/// [`parse_backup_suffix`]'s defensive stance.
pub(crate) fn backup_target_name(name: &str) -> Option<String> {
    if strip_ts_at_end(name).is_some() {
        return strip_ts_at_end(name).map(str::to_string);
    }
    // Nested ifs (not let-chains) so the crate's MSRV
    // (rust-version = "1.85") stays buildable.
    if let Some((before, ext)) = name.rsplit_once('.') {
        if let Some(stem) = strip_ts_at_end(before) {
            return Some(format!("{stem}.{ext}"));
        }
    }
    None
}

/// `<stem>_<18-char timestamp>` → `<stem>`, when the tail really is a
/// timestamp.
fn strip_ts_at_end(s: &str) -> Option<&str> {
    parse_ts_at_end(s)?;
    Some(&s[..s.len() - 19])
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
pub(crate) fn parse_human_duration(s: &str) -> Result<jiff::Span> {
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
