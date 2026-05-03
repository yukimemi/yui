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
requires the [APM](https://github.com/microsoft/apm) CLI on `PATH`
(`scoop install apm` on Windows, `brew install microsoft/apm/apm`
on macOS, `pip install apm-cli`, or `curl -sSL https://aka.ms/apm-unix | sh`).
It runs `apm install`, which compiles the
[renri](https://github.com/yukimemi/renri) skill (declared in
`apm.yml`, pinned to `#main`) into `.claude/skills/` +
`.gemini/skills/` + `.github/skills/` so AI sessions know how to
manage worktrees / jj workspaces while developing yui. Lockfile is
`apm.lock.yaml`. Pinned to `#main`, so `apm install --update`
always pulls the latest renri skill content.

## Working in this repo with AI agents

- **Read-only inspection** (browsing files, answering questions,
  running read-only commands): no worktree needed; work in the
  existing checkout.
- **Any commit-bound change** — new feature, bug fix, refactor,
  reviewer-feedback fix on an open PR: if you are on the **main
  checkout**, start with `renri add <branch-name>` and move into
  the worktree before committing (`cd "$(renri cd <branch-name>)"`,
  or use the shell wrapper from `renri shell-init` so plain
  `renri cd <name>` cds for you). If you are **already in a
  worktree** (e.g. iterating on an existing PR), keep working
  there. Do **not** edit on the main checkout for non-trivial
  changes.
- **Trivial wording / typo fixes** are the only soft exception, and
  even then `renri add` is cheap enough that defaulting to it is
  fine.

### Backend choice — jj-first

This repo is colocated git+jj. `renri add` defaults to **jj**
(creates a non-colocated jj workspace where `jj` commands work and
`git` does not — see [jj-vcs/jj#8052](https://github.com/jj-vcs/jj/issues/8052)
for why secondary colocation isn't possible yet). Stick to the
default unless there is a specific reason to use git tooling.

```sh
# In a freshly created worktree (default jj backend):
jj st                                               # status
jj describe -m "feat: ..."                          # set @-commit description
jj git push --bookmark <branch-name> --allow-new    # first push of a new branch
jj git push --bookmark <branch-name>                # subsequent pushes
```

`renri --vcs git add <branch-name>` is the override and exists for
genuine git-CLI-only needs (git submodule, native git2 tooling,
git-only hooks). Do **not** reach for it out of git-CLI familiarity
— prefer learning the equivalent jj commands.

### Cleanup after merge

After the PR merges and you've pulled the change into main:

- `renri remove <branch>` — removes a single worktree. Calls
  `git worktree remove` or `jj workspace forget` as appropriate,
  then deletes the directory. Refuses to remove the main worktree.
- `renri prune` — best-effort GC across the repo. Git: removes
  worktree metadata for already-deleted directories. jj: forgets
  workspaces whose root path is gone (the missing
  `jj workspace prune` analog).

Run `renri prune` periodically — especially after manually
`rm -rf`-ing worktree dirs without going through `renri remove`.

### Hooks in worktrees

The pre-push hook installed by `cargo make hook-install` lives in
the **main repo's** `.git/hooks/pre-push`.

- **git worktrees** share that hook directory, so plain `git push`
  from a worktree triggers `cargo make check` automatically.
- **jj workspaces** route their pushes through `jj git push`, which
  uses libgit2 directly and **does not fire git hooks**. From a jj
  workspace, run `cargo make check` manually before
  `jj git push --bookmark <branch-name>` — there is no automatic gate.

### Post-create automation (`cargo make on-add`)

`renri.toml` declares a `[[hooks.post_create]]` that runs
`cargo make on-add` immediately after `renri add` finishes. The
default chain is:

- `apm install --update` — refresh the renri skill so AI agents in
  the new worktree see the latest guidance.
- `vcs-fetch` — `jj git fetch` in a jj workspace, `git fetch`
  otherwise; cleans up subsequent rebase / merge.

Add per-repo extras (e.g. `cargo fetch`) by extending
`[tasks.on-add]`'s dependency list in `Makefile.toml`.

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
