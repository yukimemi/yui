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
  cmd/          — one module per Command variant; each loads config,
                  resolves vars, and orchestrates the underlying modules.
                  `mod.rs` re-exports so `cli.rs` still calls `cmd::apply(…)`.
    mod.rs        — module list + re-exports
    common.rs     — shared command helpers (config load, engine/ctx setup)
    apply.rs      — `yui apply`: decrypt → render → gitignore → link
    status.rs     — `yui status` output
    diff.rs       — `yui diff` (render + secret drift preview)
    render.rs     — `yui render`
    link.rs       — `yui link`
    unlink.rs     — `yui unlink`
    absorb.rs     — `yui absorb`
    secret.rs     — `yui secret init/encrypt/store/unlock`
    init.rs       — `yui init`
    list.rs       — `yui list`
    unmanaged.rs  — `yui unmanaged`
    doctor.rs     — `yui doctor`
    hooks.rs      — `yui hooks`
    update.rs     — `yui update` (self-update)
    gc_backup.rs  — `yui gc-backup`
    tests.rs      — cmd-level unit tests
  config.rs     — TOML schema, loading + Tera pre-render + multi-file merge
  vars.rs       — built-in `yui.*` vars (os/arch/host/user/source)
  paths.rs      — backup-path mirroring + timestamp-suffix utilities
  marker.rs     — `.yuilink` marker detection
  mount.rs      — `[[mount.entry]]` resolution (Tera dst, when filter)
  link.rs       — link mode resolution + cross-platform link/unlink
  render.rs     — Tera rendering of `*.tera` files + .gitignore management
  template.rs   — shared Tera engine + context builders
  absorb.rs     — drift detection + auto/ask decision
  backup.rs     — backup creation under `$DOTFILES/.yui/backup/`
  secret.rs     — age decrypt-on-apply + secrets-pipeline crypto helpers
  vault.rs      — Bitwarden / 1Password identity ferrying (secret store/unlock)
  git.rs        — `git status --porcelain` shell-out (auto-absorb gating)
  hook.rs       — `[[hook]]` script execution around apply
  icons.rs      — terminal icon character sets (nerd-font / emoji / ascii)
  updater.rs    — self-update facade over the `kaishin` library
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
- **Secrets are age-encrypted `*.age` files, decrypted on apply.** A
  `*.age` file in source is decrypted to a plaintext sibling without the
  `.age` suffix (`home/.netrc.age` → `home/.netrc`), and that sibling is
  listed in the managed `.gitignore` section so the plaintext never gets
  committed — same mechanism as `*.tera` rendered output, so the link
  walk treats it as a regular file. The **apply path is X25519-only by
  design**: it decrypts with the plain secret at `[secrets] identity`
  (default `~/.config/yui/age.txt`) and must never trigger device /
  plugin prompts. The commands are `yui secret init` (generate the
  X25519 keypair, append the public key to `[secrets] recipients`) and
  `yui secret encrypt <path>` (encrypt a plaintext to every
  `[secrets] recipients` entry → `<path>.age`). `recipients` is normally
  X25519, but hand-written passkey / plugin recipients
  (`age1yubikey1…`, `age1fido2-hmac1…`, …) are honored too, giving the
  ciphertext a parallel out-of-band decrypt path via the standalone
  `age` CLI without ever touching the apply hot path. The X25519
  identity itself travels between machines through `[secrets] vault`
  (`yui secret store` / `yui secret unlock`, backed by Bitwarden or
  1Password). See `src/secret.rs` and `src/vault.rs`. (Superseded the
  original "no secrets in MVP" decision — shipped in PR #57 / #60, drift
  detection added in #123.)
- **Secret drift hard-bails; render drift is resolved interactively.**
  When a `.tera` rendered file diverges from freshly-rendered output,
  `apply` prompts `[o]verwrite / [s]kip / [q]uit`. When a `.age`
  plaintext sibling diverges from the canonical ciphertext, `apply`
  instead `bail!`s with a `yui secret encrypt <path>` hint. The
  asymmetry is deliberate: re-rendering is cheap and idempotent, but
  silently overwriting an edited plaintext would throw away a change
  that has not yet been rolled back into the `.age` — re-encrypting is
  the heavier, explicit action the user must take.
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
## Shared conventions

This file is the agent-agnostic source of truth (per the
[agents.md](https://agents.md) convention). The matching
`CLAUDE.md` and `GEMINI.md` files are thin shims that point back
here so each tool's auto-load behaviour still finds something.
**Edit AGENTS.md, not the shims.**

### Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
- Branch names: `feat/...`, `fix/...`, `chore/...`.
- **PR titles + bodies in English. Commit messages in English.**
- **Releases are PR-driven, tagging is automatic.** Bump
  `[workspace.package].version` (workspace) or `[package].version`
  (single crate) in a `chore/release-vX.Y.Z` PR. On merge to `main`,
  `.github/workflows/auto-tag.yml` (kata-managed) detects the bump,
  pushes the `vX.Y.Z` tag, and that tag fires `release.yml` for
  binary builds + crates.io publish. **Do not run `git tag` by
  hand** — the bot tag will collide and the manual push fails.

### PR review cycle

- Every PR runs reviews from **Claude Code**
  (`.github/workflows/claude-review.yml`, kata-managed) and
  **CodeRabbit**. Wait for both bots to post, address their
  comments (push fixes to the PR branch), and merge only after
  feedback is resolved. The claude-review workflow skips
  review-exempt PRs by itself (its job-level `if:` excludes
  `chore/release-*`, `kata-apply/auto`, `apm-bump/auto`, and
  Renovate / Dependabot authors) — a missing Claude review on
  those PRs is expected, not a failure.
- **The Claude full review fires once, at PR open** (plus
  `ready_for_review` / `reopened`) — fix pushes do **not** re-trigger
  it (`synchronize` is deliberately off the trigger list; a full
  re-review per push doubled up with the mention-driven re-check
  below and burned tokens for no extra signal). Verification of
  fixes rides the `@claude` thread replies. After a large rework
  that changes the PR's shape, request a fresh full pass
  explicitly: `@claude please re-review the full PR`. CodeRabbit
  still reviews pushes on its own cadence (its app config, not
  this workflow).
- **After opening a PR, immediately enter the review-monitoring
  loop — do not ask the user whether to start it.** Drive the
  cadence with `/loop` — fixed-interval mode (e.g.
  `/loop 60s …`) schedules ticks via `CronCreate`; dynamic mode
  (no interval, `/loop …`) self-paces via `ScheduleWakeup`. The
  agent actively pulls fresh state each tick with
  `gh pr view <N> --json state,reviews,comments,statusCheckRollup`
  and `gh api repos/<owner>/<repo>/pulls/<N>/comments` (the
  latter covers inline review comments, which `gh pr view`
  does not surface) and reacts to new bot feedback. Passive
  watchers (background `gh` polls, file watchers, hooks) cannot
  trigger active follow-up, so they are not a substitute —
  without an active wake-up the agent never re-reads the PR.
- **Default polling interval: 60s.** Claude Code review /
  CodeRabbit typically reply within ~1–5 minutes of a push or
  thread reply, so a 60s tick catches them on the next wake-up
  without burning cache: 60s sits well inside the 5-minute
  prompt-cache TTL, so the conversation context stays cached
  across ticks. Do **not** stretch the interval to 300s — that
  is the worst-of-both window (you pay the cache miss without
  amortizing it). If the PR is idle but a bot re-review is still
  expected (e.g. a CodeRabbit rate-limit refill window), step
  **up** to 1200–1800s instead.
- **Stop the loop entirely when only owner approval is missing.**
  Once review bots are quiet (or quiet-by-exception — version-bump
  skip, Renovate/Dependabot skip), CI is green, and there is no
  other expected follow-up, the *only* remaining action is human
  approval. GitHub already notifies the owner; the agent
  re-entering on every cron tick to find the same "still waiting
  on owner" state burns cache and adds no value. Stop scheduling
  further wake-ups (`CronDelete` in fixed-interval mode; simply
  omit the next `ScheduleWakeup` in dynamic mode) and report the
  wait state to the user. The owner restarts the loop after their
  next push if a fresh bot pass is wanted, or merges directly.
  (A CodeRabbit rate-limit window doesn't qualify on its own — a
  re-review is still expected once the quota refills, so step up
  to 1200–1800s instead and let it ride. Stopping is only correct
  when the owner has explicitly chosen to skip the bot pass per
  the rate-limit exception below.)
- **Reply to reviewers after pushing a fix — in each thread, not
  at the top level.** Every finding lives in its own inline review
  thread; answer *each* one as an in-thread reply, carrying an
  **@-mention** (`@claude` / `@coderabbitai`). Use the review-
  comment *replies* endpoint — `gh api repos/<owner>/<repo>/pulls/<N>/comments/<comment_id>/replies -f body=…`
  (or `-F in_reply_to=<comment_id> -f body=…` on the comments
  endpoint — `body` is required there too) — and
  get each comment's `<comment_id>` from
  `gh api repos/<owner>/<repo>/pulls/<N>/comments`. A single
  top-level `gh pr comment` does **not** count: it leaves every
  inline thread unresolved, the bot can't tie your response to the
  finding it raised, and the per-finding audit trail is lost.
  Reply in-thread even when you're **declining** a suggestion —
  say why; a silent skip reads as overlooked. Note `@claude` also
  triggers the interactive responder
  (`.github/workflows/claude.yml`, kata-managed) — it will
  re-check the fix and reply on the thread. Since fix pushes no
  longer re-trigger the full review, this mention-driven re-check
  is the **only** Claude-side verification of a fix — don't skip
  it for substantive fixes; do skip it for pure FYI notes that
  need no verification.
- A review thread is **settled** the moment the latest bot reply
  is ack-only ("Thank you" / "Understood" / a re-review summary
  with no new findings) or 30 minutes elapse with no actionable
  comment.
- **Merge gate**: review bots quiet AND owner explicit approval.
- Bot-authored PRs (Renovate / Dependabot) skip the bot-review
  gate; CI green + owner approval is enough.
- **Version-bump-only PRs** (a single `chore/release-vX.Y.Z`
  branch whose entire diff is `[workspace.package].version` /
  `[package].version` + the matching inter-crate refs +
  `Cargo.lock`) **also skip the bot-review gate.** There is
  nothing for the bots to find in a version bump, and the
  release pipeline downstream of merge (auto-tag → release.yml)
  is time-sensitive. CI green + owner approval is enough.
- **Treat CodeRabbit rate-limit notices as "quiet" for the
  merge gate.** If CodeRabbit only posts a "Review limit
  reached" quota-exhaustion message (no findings, no inline
  comments), it has produced no review content — there is
  nothing to address. Re-trigger with `@coderabbitai review`
  once the quota refills if you want a real pass; for small or
  time-sensitive PRs, merge on owner approval without waiting.

### Worktree workflow

> **Before your FIRST edit to any file, run `renri add` — NEVER edit the
> main checkout.** Read-only inspection (Read / Grep / Glob) stays on the
> main checkout; the instant you intend to *change* a file, you must
> already be in a worktree. The trap that keeps catching agents: diving
> into a fix the moment the diagnosis lands and editing in place. A
> concurrent agent shares the main checkout — your in-place edits will
> clobber theirs or be clobbered, and in a jj-colocated repo a stray
> working-copy commit entangles unrelated WIP into your branch. If you
> slip and edit in the main checkout, capture the diff first (jj already
> snapshotted it into the working-copy commit, so `jj diff > patch`; for
> git, `git stash` or save a patch — if you got as far as committing on a
> branch, just push it). Then reset the main checkout to pristine main
> (`jj new main@origin`, or `git switch -`), `renri add` a worktree, and
> re-apply the captured diff there.

Use [`renri`](https://github.com/yukimemi/renri) for any
commit-bound change. From the main checkout:

```sh
renri add <branch-name> --from main@origin            # create a worktree (jj-first), off latest upstream main
renri --vcs git add <branch-name> --from origin/main  # force a git worktree, off latest upstream main
renri remove <branch-name> -y --non-interactive  # cleanup after merge (agent-safe; see note)
renri prune                        # GC stale worktrees
```

Read-only inspection can stay on the main checkout.

**Always pass `--from <upstream main>`** (`main@origin` for jj,
`origin/main` for git). Without it, `renri add` forks off the *cwd
worktree's current HEAD* — in a long-lived main checkout that often
lags upstream, so the PR later shows up CONFLICTING against a `main`
that had already moved (e.g. a refactor merged upstream before the
branch was cut), forcing a manual re-port of the whole change.
`renri add` does fetch first, but fetching only updates `main@origin`
— it never moves the checkout's HEAD, so an explicit `--from` is what
guarantees a fresh base.

**Agents / non-interactive shells:** `renri remove` prints a details
panel and waits for a confirmation prompt — without `-y` it **hangs**,
and `--non-interactive` *alone* errors asking for `-y`. Always pass
`-y`, and add `--non-interactive` so a mistyped/omitted name fails
instead of opening a fuzzy picker (the same picker-fallback applies to
`remove` / `cd` / `exec` with no name). Use `-f`/`--force` to remove a
worktree that still has uncommitted changes or conflicts. To sweep
every merged-PR worktree in one shot: `renri remove --merged -y`.

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

This repo follows the shared Rust toolchain conventions. The
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
fix to `yukimemi/pj-rust` so every Rust project using these templates picks
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

### Releasing: version bump PR + auto-tag

Releases are triggered from `main` by a Cargo.toml version
change. `.github/workflows/auto-tag.yml` is kata-managed (source:
`yukimemi/pj-rust/.github/workflows/auto-tag.yml.tera`). It
watches `main` and, whenever a commit lands that changes the
top-level `version = "..."` in `Cargo.toml`, it pushes a matching
`vX.Y.Z` tag — no manual `git tag` step is needed. The tag push
then fires `release.yml`; see `kata:agents:rust-lib:*` or
`kata:agents:rust-cli:*` for what release.yml does in each
crate shape.

Cut a release via a small PR — never `git push` the bump
straight to `main`, even though the base block lists version
bumps as an exception to "no direct push". `auto-tag.yml` only
fires on `main`-branch pushes, so the bump must land via a merge
either way; using a PR also gives CI a chance to gate the
release. Enable automerge so CI green = release start:

```sh
git switch -c chore/release-vX.Y.Z
# Edit `package.version` in Cargo.toml, then:
cargo build                     # let Cargo.lock follow
git commit -am "chore: release vX.Y.Z"
git push -u origin chore/release-vX.Y.Z
gh pr create --fill
gh pr merge --auto --squash --delete-branch
```

Once CI is green the PR auto-merges. `auto-tag.yml` then pushes
`vX.Y.Z`, which fires `release.yml`.

**Repo settings to set once:** enable
`delete_branch_on_merge=true` (Settings → General →
"Automatically delete head branches"). The `--delete-branch`
flag on `gh pr merge --auto` is effectively a no-op — gh
returns as soon as automerge is enabled, so the deletion has to
happen server-side, which requires the repo setting.

**Why `KATA_APPLY_TOKEN`:** GitHub refuses to fire downstream
workflows from tags pushed by the default `GITHUB_TOKEN`, so
`auto-tag.yml` pushes with `KATA_APPLY_TOKEN` (the same PAT
`kata-apply.yml` already uses). Each consumer repo needs a
`KATA_APPLY_TOKEN` secret set; if a version-bump merge silently
doesn't fire `release.yml`, the missing PAT is the first thing
to check.
<!-- kata:agents:rust:end -->
<!-- kata:agents:rust-cli:begin -->
### Rust CLI release flow

This is a Rust CLI crate, so the release pipeline is publish-aware.
`yukimemi/pj-rust-cli` ships a tag-driven release workflow in
`.github/workflows/release.yml` (rendered from
`release.yml.template` for the same don't-auto-execute reason
ci.yml uses).

Releases are triggered by a Cargo.toml version bump landing on
`main`. The bump flow itself (PR with automerge → `auto-tag.yml`
pushes `vX.Y.Z` → `release.yml` runs) is documented in
`kata:agents:rust:*` under "Releasing: version bump PR +
auto-tag" — that block also covers the `KATA_APPLY_TOKEN` and
`delete_branch_on_merge` setup. What `release.yml` then does for
a **CLI** crate:

1. Cross-compiles binaries for **three** targets — full triples
   `x86_64-unknown-linux-musl`, `x86_64-pc-windows-msvc`,
   `aarch64-apple-darwin`. Linux is musl (statically linked, so the
   binary runs on any glibc vintage); the Linux job installs
   `musl-tools` first. Intel Mac (`x86_64-apple-darwin`) is
   deliberately **not** built — Apple Silicon only.
2. Uploads them as a GitHub Release with auto-generated notes.
3. `cargo publish --locked` to crates.io using the
   `CARGO_REGISTRY_TOKEN` repo secret.

Set the `CARGO_REGISTRY_TOKEN` secret once per repo (`gh secret
set CARGO_REGISTRY_TOKEN`) before the first release. If the
crate is internal-only and shouldn't go to crates.io, either drop
the `publish` job locally (release.yml is `when = "once"` so the
edit survives subsequent applies) or set `package.publish = false`
in `Cargo.toml`.

The binary name is derived from the GitHub repo name at runtime
(`${{ github.event.repository.name }}`), so the workflow is
identical across CLIs using these templates unless your `[[bin]] name` in
`Cargo.toml` deliberately differs from the repo name — in that
case override `BIN_NAME` in the workflow's `env:` block.

### Release smoke target (`examples/smoke.rs`)

After `cargo build --release`, `release.yml` runs
`cargo run --release --target <T> --example smoke` on every build
matrix entry. `cargo test` runs only library code, so the produced
binary's startup path goes unverified — that's how shoka v0.10.0
shipped a rustls `CryptoProvider` panic to crates.io even though
all 13 CI checks were green.

The template's default `examples/smoke.rs` body is intentionally
no-op so kata can drop it into every consumer crate without
breaking releases. **Override it per crate** with the smallest
operation that exercises the regression-prone surface:

- HTTPS-using CLIs: build the API client (octocrab, reqwest, etc.)
  and issue a tiny no-auth GET — that forces the rustls handshake
  to run inside the same binary the release publishes.
- File-handling CLIs: write+read a temp file via the real I/O
  helpers (catches missing crate features, permission regressions).
- Pure library crates: leave as no-op.

A failing smoke blocks the release before publishing to GitHub
Releases / crates.io.
<!-- kata:agents:rust-cli:end -->
