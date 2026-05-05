# AGENTS.md

Guidance for AI agents (Claude / Codex / Gemini) working in this
repo. The yukimemi/* shared conventions live in the
`<!-- kata:agents:* -->` blocks below, sourced from
`yukimemi/pj-base` / `pj-rust` / `pj-rust-cli` via `kata apply` —
see those for git workflow, PR review cycle, build/lint/test
commands, release flow, and renri worktree usage.

The sections above the marker blocks are yui-specific and
consumer-owned: edit them freely; `kata apply` won't touch them.

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

## yui-specific tooling notes

The base / rust / rust-cli marker blocks below cover the
yukimemi/* common toolchain (cargo make, renri, jj-first
worktrees, release flow). Two repo-specific elaborations that
matter when working in yui:

### jj-first colocation

This repo is colocated git+jj. `renri add` defaults to **jj**,
which creates a non-colocated jj workspace where `jj` commands
work and `git` does not — see
[jj-vcs/jj#8052](https://github.com/jj-vcs/jj/issues/8052) for
why secondary colocation isn't possible yet. Stick to the jj
default unless there's a specific reason to use git tooling.

### Hooks in jj workspaces don't fire

The pre-push hook installed by `cargo make hook-install` lives
in the main repo's `.git/hooks/pre-push`.

- **git worktrees** share that hook directory, so plain
  `git push` from a worktree triggers `cargo make check`
  automatically.
- **jj workspaces** push via `jj git push`, which uses libgit2
  directly and **does not fire git hooks**. From a jj workspace,
  run `cargo make check` manually before
  `jj git push --bookmark <branch-name>` — there is no
  automatic gate.

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

<!-- kata:agents:base:begin -->
## yukimemi/* shared conventions

This file is the agent-agnostic source of truth (per the
[agents.md](https://agents.md) convention). The matching
`CLAUDE.md` and `GEMINI.md` files are thin shims that point back
here so each tool's auto-load behaviour still finds something.
**Edit AGENTS.md, not the shims.**

### Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
  - Exception: standalone version bumps.
- Branch names: `feat/...`, `fix/...`, `chore/...`.
- **PR titles + bodies in English. Commit messages in English.**
- Tag-based releases: `git tag vX.Y.Z && git push origin vX.Y.Z`.

### PR review cycle

- Every PR runs reviews from **Gemini Code Assist** and
  **CodeRabbit**. Wait for both bots to post, address their
  comments (push fixes to the PR branch), and merge only after
  feedback is resolved.
- **Reply to reviewers after pushing a fix.** Reply on the
  corresponding review thread with an **@-mention**
  (`@gemini-code-assist` / `@coderabbitai`). Silent fixes are
  invisible to reviewers and cost the audit trail.
- A review thread is **settled** the moment the latest bot reply
  is ack-only ("Thank you" / "Understood" / a re-review summary
  with no new findings) or 30 minutes elapse with no actionable
  comment.
- **Merge gate**: review bots quiet AND owner explicit approval.
- Bot-authored PRs (Renovate / Dependabot) skip the bot-review
  gate; CI green + owner approval is enough.

### Worktree workflow

Use [`renri`](https://github.com/yukimemi/renri) for any
commit-bound change. From the main checkout:

```sh
renri add <branch-name>            # create a worktree (jj-first)
renri --vcs git add <branch-name>  # force a git worktree
renri remove <branch-name>         # cleanup after merge
renri prune                        # GC stale worktrees
```

Read-only inspection can stay on the main checkout.

### kata-managed sections

Several files in this repo are managed by `kata apply` from the
[`yukimemi/pj-presets`](https://github.com/yukimemi/pj-presets)
templates — the bytes between `<!-- kata:*:begin -->` and
`<!-- kata:*:end -->` markers, plus the overwrite-always files
listed in `.kata/applied.toml`. **Editing those bytes locally
won't survive the next `kata apply`** — push the change to the
upstream template repo (`yukimemi/pj-base` / `yukimemi/pj-rust` /
…) instead. The marker scopes are layered:

- `kata:agents:base:*` — language-agnostic conventions (this section).
- `kata:agents:rust:*` — added when `pj-rust` applies.
- `kata:agents:rust-cli:*` — added when `pj-rust-cli` applies.
<!-- kata:agents:base:end -->
<!-- kata:agents:rust:begin -->
### Rust workflow

This repo follows the yukimemi/* Rust toolchain conventions. The
language-agnostic conventions block above (`kata:agents:base:*`)
covers git workflow, PR review cycle, and worktree usage.

### Build / lint / test

```sh
cargo make check                    # fmt --check + clippy + test + lock-check (the pre-push gate)
cargo make setup                    # one-time hook install + apm install
cargo build                         # debug build
cargo build --release               # release build
cargo test                          # tests; add -- --nocapture for stdout
```

`cargo make check` is what `.github/workflows/ci.yml` runs and what
the local pre-push hook calls — anything that passes locally
should pass on CI and vice versa. Don't paper over a failing
clippy by sprinkling `#[allow(clippy::...)]`; fix the underlying
issue or push back on the lint with reasoning.

### Toolchain pin

The Rust toolchain is pinned via `rust-toolchain.toml` and the
project compiles with the `stable` channel. Don't introduce
nightly-only features without a real reason; if you do, document
the reason in the relevant module.

### Lint / format policy

`rustfmt.toml` and `clippy.toml` are kata-managed (sourced from
`yukimemi/pj-rust`). Edits to those files in this repo won't
survive the next `kata apply`; if a setting is wrong, push the
fix to `yukimemi/pj-rust` so every yukimemi/* Rust project picks
it up.

### CI workflow

`.github/workflows/ci.yml` is also kata-managed. The source lives
in `yukimemi/pj-rust/.github/workflows/ci.yml.template` (the
`.template` suffix keeps GitHub Actions from running the source
itself in pj-rust); each Rust project receives the rendered
`ci.yml` via `kata apply`. Action versions are bumped centrally
by Renovate at `yukimemi/pj-rust` and propagate down on the next
apply, so don't bump them locally — Renovate is configured
(via the kata-distributed `renovate.json`) to ignore
`.github/workflows/ci.yml` and `.github/workflows/release.yml`
in each PJ to avoid the bump→clobber loop.
<!-- kata:agents:rust:end -->
<!-- kata:agents:rust-cli:begin -->
### Rust CLI release flow

This is a Rust CLI crate, so the release pipeline is publish-aware.
`yukimemi/pj-rust-cli` ships a tag-driven release workflow in
`.github/workflows/release.yml` (rendered from
`release.yml.template` for the same don't-auto-execute reason
ci.yml uses).

```sh
# Bump `package.version` in Cargo.toml (run `cargo build` so
# Cargo.lock follows), then:
git commit -am "chore: bump version to X.Y.Z"
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin main vX.Y.Z
```

The workflow then:
1. Cross-compiles binaries for x86_64 Linux / Windows / macOS,
   plus aarch64 macOS (Apple Silicon) — full triples
   `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
   `x86_64-apple-darwin`, `aarch64-apple-darwin`.
2. Uploads them as a GitHub Release with auto-generated notes.
3. `cargo publish --locked` to crates.io using the
   `CARGO_REGISTRY_TOKEN` repo secret.

Set the `CARGO_REGISTRY_TOKEN` secret once per repo (`gh secret
set CARGO_REGISTRY_TOKEN`) before the first tag push. If the
crate is internal-only and shouldn't go to crates.io, either drop
the `publish` job locally (release.yml is `when = "once"` so the
edit survives subsequent applies) or set `package.publish = false`
in `Cargo.toml`.

The binary name is derived from the GitHub repo name at runtime
(`${{ github.event.repository.name }}`), so the workflow is
identical across yukimemi/* CLIs unless your `[[bin]] name` in
`Cargo.toml` deliberately differs from the repo name — in that
case override `BIN_NAME` in the workflow's `env:` block.
<!-- kata:agents:rust-cli:end -->
