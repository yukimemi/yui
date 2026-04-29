# yui

> 結 — *target-as-truth* dotfiles manager

`yui` is a dotfiles manager that flips the chezmoi flow on its head: edit
your live configs (the **target**), and the source repo updates
automatically. Built to fix three chezmoi pain points:

- having to edit the source first and `apply` afterwards is a constant tax
- apps overwrite the target directly, so source and target drift
- new files apps create aren't tracked, and you can't even notice them

## How it works

The actual files live in your dotfiles repo, and the targets are
**hardlinks / junctions / symlinks** pointing at them. When an app writes
to the target, the link routes the write straight through to source — no
copy step, no drift.

| Platform | files | directories |
|----------|-------|-------------|
| Linux / macOS | symlink | symlink |
| Windows (default) | hardlink | junction |
| Windows (opt-in) | symlink | symlink (requires Developer Mode / admin) |

Drop a `.yuilink` marker file inside any directory in your source tree
to make `yui` link that whole directory as a single unit — files an
app creates inside it land directly in your source repo.

When an editor's atomic-save breaks a hardlink, `yui` looks at mtimes,
content, and git status to decide between **auto-absorb** (target →
source, with backup), **relink only** (contents identical), or **ask**
(anomaly).

## Install

```sh
cargo install yui-cli
```

Binary lives at `~/.cargo/bin/yui`. Make sure that's on your `PATH`.

## Quick start

```sh
# Initialize a fresh source repo at $DOTFILES (and install git hooks).
yui init --git-hooks

# Edit $DOTFILES/config.toml to declare your mounts, then:
yui apply        # render templates + link targets + auto-absorb drift
yui status       # show drift across all mounts
yui doctor       # diagnose your environment
```

Minimal `$DOTFILES/config.toml`:

```toml
[vars]
git_email = "you@example.com"

[[mount.entry]]
src = "home"
dst = "{{ env(name='HOME') | default(value=env(name='USERPROFILE')) }}"

[[mount.entry]]
src  = "appdata"
dst  = "{{ env(name='APPDATA') }}"
when = "{{ yui.os == 'windows' }}"
```

`config.local.toml` (gitignored) is for machine-local overrides:

```toml
[vars]
git_email = "you@work.example"
```

Built-in variables exposed to Tera: `yui.os`, `yui.host`, `yui.user`,
`yui.arch`, `yui.source`. Environment variables are read with
`{{ env(name='HOME') }}` (Tera's standard function).

## Status

Pre-release. MVP design complete; implementation in progress.

## License

MIT
