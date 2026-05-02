# CLAUDE.md

Guidance for Claude Code when working in this repo.

## What yui is

A *target-as-truth* dotfiles manager, written in Rust. Inverts the
chezmoi model: source and target share inode (hardlink / junction / symlink),
so editing the target is editing the source. When an editor's atomic-save
breaks the hardlink, `yui` auto-absorbs the divergence rather than
forcing the user to remember a manual `re-add`.

Name comes from 結 (yui, "tie / bind"). Crate name `yui-cli`,
binary `yui`, repo `yukimemi/yui` (`yui` itself is taken on crates.io
by an unrelated abandoned crate).

## Source layout

```
src/
  main.rs       — entry point, parses CLI and dispatches to lib::cli::Cli::run
  lib.rs        — module list + tracing init
  cli.rs        — clap definitions (Cli, Command enum, run())
  cmd.rs        — one function per Command variant; each loads config,
                  resolves vars, and orchestrates the underlying modules
  config.rs     — TOML schema, loading + Tera pre-render + multi-file merge
  vars.rs       — built-in `yui.*` vars (os/arch/host/user/source)
  paths.rs      — backup-path mirroring + timestamp-suffix utilities
  marker.rs     — `.yuilink` marker detection
  mount.rs      — `[[mount.entry]]` resolution (Tera dst, when filter)
  link.rs       — link mode resolution + cross-platform link/unlink
  render.rs     — Tera rendering of `*.tera` files + .gitignore management
  absorb.rs     — drift detection + auto/ask decision
  backup.rs     — backup creation under `$DOTFILES/.yui/backup/`
  status.rs     — `yui status` output
  error.rs      — Error / Result types
tests/
  cli.rs        — integration tests via `assert_cmd`
```

## Key design decisions (don't rediscover)

These were settled during the initial design pass; flag with the user
before reverting any of them.

- **target is the source of truth.** Source files are linked into the
  target via hardlink (Windows file), junction (Windows dir), or symlink
  (Unix). Editing the target is editing the source. If a hardlink is
  broken (e.g. by an editor's atomic save), the difference is auto-absorbed
  back into source — that's the whole point of the tool.
- **default link mode is `auto`** — Unix: symlink for everything,
  Windows: hardlink for files + junction for dirs. The Windows defaults
  avoid requiring Developer Mode / admin. `symlink` is opt-in for
  Windows users who do want it.
- **`.yuilink` marker decides where junctions land.** A directory
  containing the marker is junctioned as a unit and recursion stops.
  Without the marker, `yui` recurses and hardlinks individual files.
  This is so apps creating new files inside a junctioned dir land
  directly in source (no "untracked" detection needed for that case).
- **Templates are `*.tera` files; rendered output goes to the *same
  directory* as the template.** `home/.gitconfig.tera` →
  `home/.gitconfig`. Rendered files are listed in a managed section
  (`# >>> yui rendered ... <<<`) of `.gitignore` so they don't get
  committed. This is what lets templates work *inside* junctioned
  directories — apps see both the template and the rendered file
  through the junction, but only the rendered one is what they care
  about.
- **Rendered files are NOT git-tracked.** They diverge per-OS and
  would constantly conflict. The `.tera` source is the authority; the
  rendered file is a local cache. `yui render` checks "current rendered
  vs newly-rendered output" before overwriting and aborts on drift,
  so a user manual-edit isn't silently clobbered.
- **Conditional render is dual-source.** Both file-header
  `{# yui:when env.os == 'windows' #}` (Tera comment, doesn't appear in
  output) AND `[[render.rule]] match=... when=...` in config are
  honored; if both present, AND. File-level is for self-documenting
  per-template gating; config-level is for cross-cutting patterns.
- **`yui.*` is the built-in namespace** (NOT `env.*`). `env(name='X')`
  is Tera's standard function for environment variables, so `env.os`
  would read like an env var. `yui.os` / `yui.host` / `yui.user` /
  `yui.arch` / `yui.source` mirror chezmoi's `.chezmoi.*` convention.
- **Config layout fixes the `$DOTFILES` directory only.** Files at
  `$DOTFILES/config.toml`, `$DOTFILES/config.*.toml` (alphabetical),
  and `$DOTFILES/config.local.toml` (last/highest priority). No
  `~/.config/yui/` fallback — keeping the location single avoids
  "which one is the real config?" confusion. Each file is Tera-rendered
  before TOML parse so conditionals on whole sections work.
- **machine-local data is `config.local.toml` `[vars]`**, not a
  separate `data.toml`. Simpler and one less file to remember.
- **No secret/encryption support in MVP.** If users need it, point
  them at `1password` CLI or `pass` from inside a Tera template.
- **Profiles are `[vars]` switches, not a `--profile` flag.** Branch on
  `vars.work_mode` or `vars.host` inside templates / `when` clauses.
  Single repo per user.
- **auto-absorb logic** classifies on (mtime, content, git-clean):
  - target newer + content same → relink only
  - target newer + content differs → auto-absorb (backup source, copy target → source, relink)
  - source newer + content differs → anomaly, diff + ask (or skip/force per `[absorb] on_anomaly`)
  - source repo dirty (uncommitted) and `require_clean_git=true` → escalate "auto-absorb" to "ask"
  - target missing → restore from source
- **backup path scheme**: mirror absolute target into
  `$DOTFILES/.yui/backup/<abs-path>` with the drive colon stripped on
  Windows (`C:\…` → `C/…`), then suffix the basename with the timestamp.
  Files keep their extension (`bar_<ts>.yml`); dotfiles and
  no-extension files get the suffix appended directly
  (`.gitconfig_<ts>`); directories are recursive-copied into
  `<dirname>_<ts>/`.
- **`apply` is the default workflow**: render → link (creates / relinks /
  resolves drift via auto-absorb). `render`, `link`, `absorb` are
  exposed for partial workflows. Every command takes `--dry-run`.
- **Existing target-side files**: `apply` backs them up under
  `.yui/backup/` and replaces with the link. No prompt — auto-absorb
  later if the user wants the old content back. (Their content is
  preserved in backup; recovery is a `cp` away.)
- **Git ops shell out to `git`**, not `git2`. We only need
  `status --porcelain` / a few other read-only calls; `libc`-linking
  `libgit2` on Windows is more pain than it's worth.

## Development

**Practice TDD.** Red-green-refactor.

```bash
cargo make setup                    # one-time on clone: pre-push hook + APM install
cargo test                          # unit + integration
cargo test --test cli               # integration only
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo make check                    # all of the above (pre-push gate)
```

`cargo make check` mirrors CI. A pre-push hook should be installed on
checkout so failed checks block push.

`cargo make setup` is `hook-install` + `apm-install`. The latter
runs `apm install`, which compiles the
[renri](https://github.com/yukimemi/renri) skill (declared in
`apm.yml`, pinned to `#v0.1.5`) into `.github/skills/` so AI
sessions know how to manage worktrees / jj workspaces while
developing yui. Lockfile is `apm.lock.yaml`. Bump the pinned version
explicitly when wanting newer renri features.

## Resilience principle

Borrowed from rvpm: a single failure should not stop the whole tool.
- Source repo not detected → clear error with `yui doctor` hint.
- One mount entry's `dst` Tera-render fails → warn, skip that entry,
  continue the others.
- One template render fails → warn, skip that template, continue.
- One link operation fails → warn, continue with siblings, surface the
  full failure set at the end.
- `yui status` should still work even if `yui apply` would fail.

`yui doctor` exists to surface environmental problems (no symlink
permission on Windows, broken `$DOTFILES`, dirty git, stale backups)
*before* the user runs `apply`.

## Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
  - Exception: standalone version bumps (`Cargo.toml` + `Cargo.lock`
    refresh + `git tag vX.Y.Z`) — a one-line bump PR is more noise
    than signal.
- Branch names describe the change (`feat/...`, `fix/...`).
- **PR titles + bodies in English. Commit messages in English.**
- Tag-based releases: `git tag vX.Y.Z && git push origin vX.Y.Z`.

### PR review cycle

- Every PR triggers **Gemini Code Assist** and **CodeRabbit** reviews.
  Wait for both, address comments (push fixes to the PR branch), and
  merge only after feedback resolves.
- **Reply to the reviewer after pushing a fix.** Post a reply in the
  comment thread with `@gemini-code-assist` / `@coderabbitai` so the
  bot knows the feedback was acted on. Silent fixes lose the audit
  trail and trigger blind re-review.
- **Watch actively after fix + reply.** Poll `gh pr view` /
  `gh api .../pulls/<n>/comments` every ~5 min until bots stop
  posting actionable comments. New actionable comments restart the
  loop. Use `/loop` or `ScheduleWakeup` for automation.
- **Settle rule**: a thread settles when the latest bot reply is
  ack-only ("Thank you", "Understood", a re-review summary with no
  new findings). New actionable comments un-settle it.
- **Stop conditions**:
  1. All open threads settled — PR is quiet, ask owner to merge.
  2. No bot reply for 30 min after the last actionable comment —
     timeout that thread as settled.
- **Merge gate**:
  1. Review bots stopped posting actionable comments.
  2. @yukimemi has explicitly approved.
- **Bot-authored PRs (Renovate / Dependabot)**: review bots skip them
  by default, so the "wait for review" gate doesn't apply. Merge if
  CI is green and owner approves.

## Useful invocations

```sh
# Apply with dry run
cargo run --quiet -- apply --dry-run

# Status
cargo run --quiet -- status

# Override source for experiments
YUI_SOURCE=/tmp/exp-dotfiles cargo run --quiet -- apply --dry-run

# Diagnose environment
cargo run --quiet -- doctor
```

## Version + changelog

Version lives only in `Cargo.toml`. `cargo check` refreshes
`Cargo.lock` after a bump. Commit titles follow
`<type>: <summary> (vX.Y.Z)` (e.g. `feat: ... (v0.1.0)`) so the
release surface is traceable from `git log`.
