<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg" />
    <img src="assets/logo.svg" width="560" alt="yui — target-as-truth dotfiles manager" />
  </picture>
</p>

<p align="center">
  <b>結 — edit your live configs, the source repo updates itself.</b>
</p>

<p align="center">
  <a href="https://crates.io/crates/yui-cli"><img src="https://img.shields.io/crates/v/yui-cli.svg" alt="crates.io"/></a>
  <a href="https://github.com/yukimemi/yui/actions/workflows/ci.yml"><img src="https://github.com/yukimemi/yui/actions/workflows/ci.yml/badge.svg" alt="CI"/></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT"/></a>
</p>

`yui` flips the chezmoi flow: instead of editing your source repo and
running `apply` to push changes out to `~`, you edit `~` directly and
the source follows automatically. The two sides share a backing inode
(hardlink / junction / symlink), so an app's write to the target *is*
a write to source.

It exists to fix three chezmoi pain points the author hit running
chezmoi for years:

1. **The edit-source-then-apply tax** — every config tweak became a
   two-step ceremony.
2. **Source ↔ target drift** — apps overwrite the target directly,
   and the user finds out at the next `chezmoi diff`.
3. **Untracked new files** — apps that create new files inside a
   managed directory aren't visible to chezmoi unless you remember
   to `chezmoi add` them.

## How it works

Your dotfiles repo is a normal directory tree. `yui apply` walks it
and links each file/directory into its target location:

| platform | files | directories |
|----------|-------|-------------|
| Linux / macOS | symlink | symlink |
| Windows (default) | **hardlink** | **junction** |
| Windows (opt-in) | symlink | symlink (Developer Mode / admin) |

The Windows defaults are deliberate: hardlinks and junctions both
work without elevated permissions and survive most editors' "atomic
save" rename trick. When that trick *does* break the hardlink, the
**absorb classifier** notices on the next `apply` / `status`:

```
target's file-id == source's file-id?            → InSync
content identical, different file-id?            → RelinkOnly
target newer + content differs?                  → AutoAbsorb (target wins)
source newer + content differs?                  → NeedsConfirm (anomaly)
target missing?                                  → Restore
```

`AutoAbsorb` backs source up under `$DOTFILES/.yui/backup/` and
copies target's content into source before relinking — your local
edit is preserved, even when an editor saved over the link.

## Install

```sh
cargo install yui-cli
```

Pre-built binaries for Linux x86_64, Windows x86_64, and macOS
(Intel + Apple Silicon) are attached to every
[GitHub Release](https://github.com/yukimemi/yui/releases).

## Quick start

```sh
# Scaffold a source repo at the current directory and install git hooks.
yui init --git-hooks

# Edit $DOTFILES/config.toml to declare your mounts, then:
yui apply        # render templates + link targets + auto-absorb drift
yui list         # see every src→dst mapping at a glance
yui status       # check what drifted
yui doctor       # environment sanity check
```

Smallest useful `$DOTFILES/config.toml`:

```toml
[[mount.entry]]
src = "home"
dst = "~"          # ~ expands to $HOME / $USERPROFILE per OS

[[mount.entry]]
src  = "appdata"
dst  = "{{ env(name='APPDATA') }}"
when = "yui.os == 'windows'"
```

Add files under `home/` and they'll link into `~`. Add a `.yuilink`
file to a directory to junction the whole directory as one unit (so
files an app creates inside that dir land back in source
automatically).

## Templates (`*.tera`)

Files ending in `.tera` are rendered with [Tera] before linking; the
output is a sibling file with the `.tera` suffix dropped. `yui` adds
the rendered file to a managed `# >>> yui rendered (auto-managed) <<<`
section of `.gitignore` so it doesn't get committed.

```
home/.gitconfig.tera   →  home/.gitconfig   →  ~/.gitconfig
```

Templates have access to `yui.os` / `yui.host` / `yui.user` /
`yui.arch` / `yui.source` and your `[vars]` table. Per-host overrides
go in `config.local.toml` (machine-local, gitignored), which `yui`
loads after `config.*.toml` so its values win.

[Tera]: https://keats.github.io/tera/

## One source → many targets

If you want the same source directory linked to different places on
different OSes — common for editor configs (`~/.config/nvim` on Unix,
`%LOCALAPPDATA%\nvim` on Windows) — drop a `.yuilink` with content:

```toml
# $DOTFILES/home/.config/nvim/.yuilink
[[link]]
dst = "~/.config/nvim"

[[link]]
dst = "{{ env(name='LOCALAPPDATA') }}/nvim"
when = "yui.os == 'windows'"
```

`yui list` shows each link and which `when` would activate it.

### Stacking markers and file-level entries

Markers compose. A parent `.yuilink` no longer stops the walker, so
you can junction a whole `~/.config` and *also* layer extra dsts onto
specific subdirs:

```toml
# $DOTFILES/home/.config/.yuilink — junction the whole .config dir
[[link]]
dst = "~/.config"
```

```toml
# $DOTFILES/home/.config/nvim/.yuilink — extra Windows-only dst
[[link]]
dst = "{{ env(name='LOCALAPPDATA') }}/nvim"
when = "yui.os == 'windows'"
```

Both links land — the parent takes care of the natural placement, the
child adds its OS-specific alternate.

A `[[link]]` may also carry a `src = "<filename>"` to scope the link to
a single sibling file rather than the directory itself. Useful for
paths that don't follow `~/.config/<app>/` conventions, like the
PowerShell profile on Windows:

```toml
# $DOTFILES/home/.config/powershell/.yuilink
[[link]]
src = "Microsoft.PowerShell_profile.ps1"
dst = "{{ env(name='USERPROFILE') }}/Documents/PowerShell/Microsoft.PowerShell_profile.ps1"
when = "yui.os == 'windows'"
```

`src` must be a single filename (no path separators); the file lives
right next to the marker. The rest of the directory still falls
through to whatever placement an ancestor (or the parent mount)
provides.

## `.yuiignore` — exclude paths from being linked

A `$DOTFILES/.yuiignore` file (gitignore syntax) keeps matched paths
out of every yui flow — render skips them, list omits them, and apply
won't link them. Useful for editor lock-files, build artifacts, OS
junk like `.DS_Store`, and anything else that lives next to your real
config but shouldn't be propagated:

```gitignore
# $DOTFILES/.yuiignore
**/.DS_Store
**/lock.json
home/.config/nvim/lazy-lock.json     # exact path also works

# Exclude all of build/ except the one file we DO want linked
build/
!build/result.toml
```

Currently only the repo-root `.yuiignore` is honored — nested
`.yuiignore` files inside subdirectories are not yet walked, so put
all your rules at the top.

## Hooks — run scripts around `apply`

Drop a script under `$DOTFILES/.yui/bin/` and reference it with a
`[[hook]]` entry. The script stays a normal executable (you can run
it directly without yui); yui just decides *when* to invoke it.

```toml
[[hook]]
name   = "brew-bundle"
script = ".yui/bin/brew-bundle.sh"
# Defaults: command="bash", args=["{{ script_path }}"], when_run="onchange", phase="post"
when   = "yui.os == 'macos'"
```

Schema:

| field | required? | default | meaning |
|---|---|---|---|
| `name` | ✓ | — | unique identifier (state-tracking key, `yui hooks run <name>`) |
| `script` | ✓ | — | path to script, relative to `$DOTFILES` |
| `command` | | `"bash"` | Tera-templated interpreter |
| `args` | | `["{{ script_path }}"]` | Tera-templated each |
| `when_run` | | `"onchange"` | `once` \| `onchange` \| `every` |
| `phase` | | `"post"` | `pre` \| `post` (around `apply`) |
| `when` | | _always_ | optional Tera bool predicate |

`onchange` re-runs whenever the script's SHA-256 differs from the
last successful run. State is tracked in `$DOTFILES/.yui/state.json`
(gitignored — it's per-machine).

`command` and each `args` element are Tera-rendered with the standard
`yui.*` / `vars.*` / `env(...)` plus these extras for the script:
`script_path`, `script_dir`, `script_name`, `script_stem`,
`script_ext`. Example with a Deno script:

```toml
[[hook]]
name = "denops-build"
script = ".yui/bin/build.ts"
command = "deno"
args = ["run", "-A", "{{ script_path }}"]
when_run = "onchange"
phase = "post"
when = "yui.os != 'windows'"
```

Manual control:

```sh
yui hooks list                    # what's configured + last-run state
yui hooks run                     # run all hooks per their rules
yui hooks run brew-bundle         # run just this one (still honors `when`)
yui hooks run brew-bundle --force # bypass when_run state check
```

`apply` runs `pre` hooks before render/link and `post` hooks after
all linking. A failing hook stops `apply` immediately — fix the
script, then re-run.

## Anomalies and the `[absorb]` policy

When source AND target both diverge from each other, `yui` can't
auto-merge. It defers to your `[absorb] on_anomaly` setting:

```toml
[absorb]
auto              = true     # auto-absorb on any AutoAbsorb classification
require_clean_git = true     # treat dirty source as anomaly
on_anomaly        = "ask"    # "ask" | "skip" | "force"
```

- `ask` — on a TTY, render the diff and prompt y/N; off-TTY, skip
- `skip` — log a warning and leave both sides untouched
- `force` — treat the anomaly as auto-absorb anyway (target wins)

Need to absorb a single file regardless of policy? `yui absorb
<target-path>` does that — bypasses `auto`, `require_clean_git`, and
`on_anomaly` for an explicit user-initiated pull.

## Commands

| | |
|---|---|
| `yui init [--git-hooks]` | scaffold `config.toml` + `.gitignore` in cwd |
| `yui apply [--dry-run]` | render → link → auto-absorb |
| `yui render [--check] [--dry-run]` | template-only pass; `--check` fails on drift |
| `yui link [--dry-run]` | alias for apply (kept for muscle memory) |
| `yui list [--all] [--icons MODE] [--no-color]` | every src→dst mapping |
| `yui status [--icons MODE] [--no-color]` | drift overview, exits non-zero on any divergence |
| `yui absorb <target> [--dry-run]` | manually pull a single target into source |
| `yui unlink <path>...` | tear down a specific link |
| `yui doctor` | environment sanity check |
| `yui gc-backup [--older-than DUR]` | clean old backups (**not yet implemented** — calling it errors out) |
| `yui hooks list` | show configured `[[hook]]` entries + last-run state |
| `yui hooks run [<name>] [--force]` | run hooks on demand (bypassing `when_run` with `--force`) |

`--icons` accepts `unicode` (default), `nerd` (Nerd-Font glyphs),
`ascii` (CI-log-safe). The `[ui] icons = "..."` config key sets it
globally.

## Status

`v0.4.0` ships the absorb story end-to-end — chezmoi-replacement
ready for simple repos. Known gaps:

- no built-in encryption (use `pass` / `1password-cli` from a Tera
  template instead)
- chezmoi name-prefix translation (`dot_zshrc` → `.zshrc`,
  `run_once_*.sh.tmpl`) is **not** implemented — bring-your-own
  rename when migrating

Migration from chezmoi: rename files (`dot_X` → `.X`), convert
`*.tmpl` → `*.tera` (Go template → Tera syntax), and pull the
`run_*` scripts into a separate runner you trigger yourself. yui has
no opinion on what runs them.

## License

MIT
