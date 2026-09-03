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
  links.rs      — `[[link]]` entry schema shared by `config.toml` +
                  `.yuilink`, plus the per-dir plan the walkers consult
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
- **`[[link]]` decides where dir-level links land**, declared either in
  `config.toml`'s central table (`src` required, relative to
  `$DOTFILES`) or in a `<dir>/.yuilink` marker (`src` optional, relative
  to the marker's dir). A dir-scoped entry links that directory as a
  unit; without one, `yui` recurses and links individual files. The
  point of dir-level linking is that apps creating new files inside the
  linked dir land directly in source (no "untracked" detection needed
  for that case). Both spellings exist on purpose: the central table
  keeps the tree free of marker files (which are also visible from the
  target side once the dir is junctioned), while a marker travels with
  its directory and can't rot into a dangling entry.
- **Markers do not stop the walk (v0.6+).** It keeps descending and
  aggregates entries, so a descendant *adds* destinations on top of its
  ancestors. Exact duplicates (same `src` / `dst` / `when`) collapse so
  declaring one mapping twice doesn't double the work or the report
  rows. Marker discovery is gated on `MountStrategy::Marker`; central
  entries are explicit instructions and apply under `per-file` too.
- **Link modes live in `[mount]`** (`file_mode` / `dir_mode`). They used
  to be a `[link]` table; that key is now the `[[link]]` array, and
  `config::load` intercepts the old table shape with a migration error
  rather than letting serde emit a type error about a sequence. A single
  `[[link]]` entry may override the mechanism with `mode` — legal values
  depend on what it links (dirs: `auto`/`symlink`/`junction`, files:
  `auto`/`symlink`/`hardlink`), validated where the entry is declared.
  The override exists because flipping `dir_mode` globally on Windows
  costs every link its privilege-free default just to accommodate one
  junction-hostile app. The effective mode is passed down the link /
  absorb call chain as an argument — no ambient state, so a nested
  per-file conflict inside a dir merge can't pick up the dir's override.
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
- **Dir-level relinks stage the target aside; they never delete in
  place.** `absorb_target_dir_into_source` /
  `overwrite_source_dir_into_target` rename the target to a sibling
  (`<name>.yui-absorb.<ts>` / `<name>.yui-discard.<ts>`), put the link
  up immediately, and only then merge / `remove_dir_all` the staged
  tree. Rationale: `remove_dir_all` on a real `~/.config` is O(tree)
  and non-atomic, so the old "merge → delete → link" order left a
  half-deleted target and no link if it was interrupted. Sibling
  placement is mandatory — `rename` is only atomic within one volume,
  which is also why the staging dir is not under `.yui/backup/`.
  A refused rename (open handles on Windows) warns and falls back to
  the in-place teardown.
- **The staging name is the crash journal.** `Absorb` means "content
  may not have reached source yet → merge, then delete"; `Discard`
  means "already in `.yui/backup/` → delete unread". Each invariant is
  established by the rename itself (the overwrite path backs up
  *before* staging precisely so `Discard` holds), so recovery needs no
  state file. `recover_staged` runs at the top of
  `link_dir_with_backup`, **before `absorb::classify`** — a recovered
  target reads `InSync`, so classifying first would strand the
  leftover. Recovery is idempotent: re-merging an already-merged tree
  is per-file content-classified, hence a no-op.
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
- **Releases are PR-driven and tagging is automatic** — in repos that
  ship a release pipeline. Bump the version in the project's own
  manifest in a `chore/release-vX.Y.Z` PR; on merge to `main` the
  language layer's `auto-tag.yml` detects the bump, pushes the
  `vX.Y.Z` tag, and that tag is what fires `release.yml`. **Do not run
  `git tag` by hand** — the bot tag will collide and the manual push
  fails. The specifics belong to the layers shipping those two
  workflows, which are not the same layer: `kata:agents:rust:*` for
  which file holds the version and for `auto-tag.yml`,
  `kata:agents:rust-{cli,lib}:*` for what `release.yml` builds and
  publishes. A repo with no `auto-tag.yml` has no release pipeline at
  all: nothing tags, and the version field in its manifest may well
  be decoration.

### Pre-merge review

Review happens **before the pull request, on the operator's machine**,
via [magi](https://github.com/yukimemi/magi). This layer no longer
ships PR-side review bots: `claude-review.yml` and `claude.yml` were
removed from it. Their scope was
human-authored PRs — their own job-level `if:` already excluded
`chore/release-*`, `kata-apply/auto`, `apm-bump/auto` and
Renovate / Dependabot — which is exactly the set magi reviews, so
keeping them meant reviewing the same diff twice, a
`CLAUDE_CODE_OAUTH_TOKEN` secret per repository, Actions minutes on
private repos, and one trap that silently cost reviews: a PR editing
either workflow was skipped by `claude-code-action`'s
workflow-validation check and merged with a green check and no
review attached.

**"Removed" is a statement about this template layer, not about
every repo's current state.** Dropping a `[[file]]` entry stops kata
from managing the rendered file — it does not delete it. A repo that
had these workflows before this change keeps `claude-review.yml` /
`claude.yml` (and the `CLAUDE_CODE_OAUTH_TOKEN` secret) under
`.github/workflows/` until someone deletes them by hand, and until
then they still fire on every human-authored PR. Check
`.github/workflows/` before treating a PR as unreviewed-except-magi:
if either file is still there, its comments are a real review, not
noise to ignore.

- **`magi review <branch>`** runs only the review + verification +
  gate half of magi's graph: nothing competes, no implementation, no
  judging, no vote. That is the mode for hand-written work.
  `magi run "<task>"` is the full competition, for work handed over
  whole. Both end at the same gate.
- What the loop actually does: each reviewer gets its **own detached
  worktree pinned at the commit under review** (no reviewer can
  perturb the tree, and the fixer never races one); `verify.e2e` runs
  in the branch's worktree and its output is fed to the fixer;
  finding ids (`R2-1-3`) are assigned by magi, not by the agent, so
  the fixer's adoption report can be matched against them; the loop
  is bounded by `review_rounds`; `verify.gate` must exit 0 before any
  merge is attempted.
- **`magi.toml` is repo-owned, not kata-managed.** Point
  `verify.gate` at the exact command CI runs, so a local pass means a
  green PR, and point `verify.e2e` at the invocation that actually
  covers the repo — feature flags included. A gate that differs from
  CI turns a clean magi run into a red PR, which is the one failure
  this arrangement cannot absorb.
- **If you did not run magi, the change was not reviewed, and nothing
  will tell you.** Do not open a PR for a hand-written change before
  `magi review` comes back clean; if you must, say so in the PR body
  and say why. What does *not* count as a substitute: a green CI run
  (it compiles and tests, it does not review), and CodeRabbit's
  silence.
- **CodeRabbit stays installed and is not part of the gate.** It does
  not auto-review repositories under 10 stars — the common case here —
  so treat it as absent unless it posts. When it does post, its
  findings are a real review: address them, reply **in the inline
  thread** with an `@coderabbitai` mention (the review-comment
  *replies* endpoint,
  `gh api repos/<owner>/<repo>/pulls/<N>/comments/<id>/replies -f body=…`),
  and reply even when declining — say why, because a silent skip
  reads as overlooked. A "review limit reached" quota notice carries
  no findings and counts as quiet; re-trigger with
  `@coderabbitai review` when the quota refills if you want a real
  pass.
- **Read the report, not the exit status.** A reviewer seat that
  times out is logged as `WARN agent timed out seat=review-2` and
  then summarised as "raised 0 finding(s)" — indistinguishable from a
  genuinely clean pass in both the summary and `magi stats`. Check
  for timeouts before believing a clean round: a round where half the
  panel never answered is not a clean round.
- **Review artifacts stay local.** magi comments on a pull request
  only when it *stops* landing one. Findings, the fixer's adoption
  report and reviewer precision live in the run directory
  (`magi show`, `magi stats`). When the PR needs a record — a
  non-obvious fix, a finding declined with an argument — paste that
  part into the PR body or a comment yourself.
- With `merge = "pr"`, magi opens the pull request and keeps going:
  watches the checks, reads the review comments (human and bot), runs
  a bounded fix round when either is unhappy, pushes, and asks before
  merging. `land_approval` is on by default and **silence is a
  hold** — nothing merges unanswered. `magi answer` (or the web UI)
  is where it asks. Out of rounds leaves the PR open with a comment
  saying what still fails; `checks: unknown` never merges.
- **Merge gate**: magi's gate green — or CI green for a change magi
  never touched — **and** every review that did post resolved (a
  leftover `claude-review.yml`, CodeRabbit, a human) **and** the
  owner's explicit approval. The irreversible step stays a human
  decision.
- **No review-monitoring poll loop for bots this layer no longer
  ships.** The old loop existed to wait on them. Where a repo still
  has `claude-review.yml` (see above) the old cadence still applies
  until it is deleted; otherwise, after opening a PR wait for CI and
  report the wait state to the owner. When magi is landing the PR
  (`land = true`), magi does the watching.
- Bot-authored PRs (Renovate / Dependabot) need no review pass at
  all: CI green + owner approval.
- **Version-bump-only PRs** — a single `chore/release-vX.Y.Z` branch
  whose entire diff is `[workspace.package].version` /
  `[package].version` plus the matching inter-crate refs and the
  lockfile — likewise. There is nothing in a version bump for a
  reviewer to find, and the release pipeline downstream of merge
  (auto-tag → `release.yml`) is time-sensitive.

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
…) instead.

The marker scopes are layered, one per applied layer:
`kata:agents:base:*` is this section, and each layer adds its own
(`kata:agents:rust:*`, `kata:agents:rust-cli:*`,
`kata:agents:pnpm:*`, `kata:agents:firebase:*`, …). Which ones apply
*here* is a grep away: `<!-- kata:` in this file.

### This project's own conventions

Everything a layer ships is generic by construction: it describes the
stack the template assumed, not what this repo grew into. **Bytes
outside every marker pair are yours and survive `kata apply`** — so
project-specific conventions belong in a section of their own, outside
the markers (conventionally at the end of the file; if a later layer
appends its block below yours, no matter — kata only ever rewrites
between its own markers). Same mechanism as the `.gitignore` /
`.gitattributes` blocks.

Write those conventions down there rather than leaving them in one
agent's head, in commit archaeology, or in a README the agent will not
read. What earns a line:

- **Any layer default that does not hold here.** A layer states its
  assumption flatly ("Hosting is the primary target", "these rules are
  a placeholder to replace"). When the project has diverged, say so and
  say why — the layer's text keeps asserting the opposite on every
  apply, and an agent that only reads the blocks will act on it.
- **Facts duplicated across files with no compiler in between** — an
  address or a path that appears in code *and* in a rules/config file
  that cannot import it, a timeout that has to stay inside another
  timeout. List every copy, so the next edit finds them all.
- **kata-shipped files this project deleted on purpose**, together with
  the `once_applied = true` line in `.kata/applied.toml` that keeps
  them deleted. Otherwise someone helpfully restores one.
- **Shapes the runtime forces but no tool checks** — an export form a
  platform requires, import specifiers that must (or must not) carry a
  file extension, a directory whose contents are reachable by URL.
- **Invariants that money or access rest on**, naming the file and line
  that actually enforces them.
- **Which language the code speaks versus what a user reads**, when the
  two differ.

A repo whose `AGENTS.md` is nothing but kata blocks is a repo where
every agent re-derives all of that from scratch — and gets the layer
defaults wrong the same way each time.
<!-- kata:agents:base:end -->
<!-- kata:agents:rust:begin -->
### Rust workflow

This repo follows the shared Rust toolchain conventions. The
language-agnostic conventions block above (`kata:agents:base:*`)
covers git workflow, PR review cycle, and worktree usage.

### Build / lint / test

```sh
cargo make check                    # editorconfig-check + fmt --check + clippy + test + lock-check (the pre-push gate)
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

**In a workspace, the version is in more than one place.** A member
that is published and depended on by another member is declared
with both a `path` and a `version` — crates.io needs a
requirement it can resolve for somebody who is not building from
the checkout, so a bare `path` will not do:

```toml
my-core = { path = "crates/my-core", version = "0.4.2" }
```

That literal does not follow `[workspace.package] version`.
Nothing in Cargo makes it, and the release above will not either.

**It fails late and quietly.** `version = "0.4.2"` means `^0.4.2`,
so a stale pin keeps resolving through every *patch* release and
stops only at the first bump that crosses the minor — where
`cargo build` refuses with `candidate versions found which didn't
match`, in the middle of cutting the release. Two repos on these
templates hit exactly this, one of them three releases after its
pins were last correct, and the other had already written the
hazard down in prose and drifted anyway.

So bump the pins in the same commit, keep them in
`[workspace.dependencies]` rather than in each member, and assert
it rather than remembering it. A test is the cheapest place —
`cargo test` already runs in CI, and it needs no toolchain a Rust
workspace does not have. [pj-rust-workspace's
README](https://github.com/yukimemi/pj-rust-workspace#the-internal-version-pin-and-the-check-for-it)
carries one to copy into any member's
`tests/check_versions.rs`: `internal_pins_match_the_workspace_version`
fails when a pin and the workspace version disagree, and
`members_inherit_the_workspace_version` fails when a member writes
its own version or reaches for a sibling by path.

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
