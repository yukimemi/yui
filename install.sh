#!/bin/sh
# Install the latest release without Rust or a package manager.
set -eu

main() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os/$arch" in
        Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
        Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-musl ;;
        *)
            printf 'Unsupported platform: %s/%s\nSee https://github.com/yukimemi/yui/releases or build with cargo install yui-cli.\n' "$os" "$arch" >&2
            exit 1
            ;;
    esac

    for cmd in curl tar mktemp; do
        if ! command -v "$cmd" >/dev/null 2>&1; then
            printf 'Required command not found: %s\n' "$cmd" >&2
            exit 1
        fi
    done

    install_dir="$HOME/.local/bin"
    tmp=$(mktemp -d)
    staged=
    trap 'rm -rf "$tmp"; if [ -n "$staged" ]; then rm -f "$staged"; fi' EXIT
    trap 'exit 1' HUP INT TERM

    url="https://github.com/yukimemi/yui/releases/latest/download/yui-$target.tar.gz"
    printf 'Downloading the latest yui for %s...\n' "$target"
    curl -fsSL "$url" -o "$tmp/yui.tar.gz"
    tar -xzf "$tmp/yui.tar.gz" -C "$tmp" yui
    chmod 755 "$tmp/yui"
    "$tmp/yui" --version

    # Stage on the destination filesystem; a failed download never touches yui.
    mkdir -p "$install_dir"
    staged=$(mktemp "$install_dir/.yui.XXXXXXXX")
    cp "$tmp/yui" "$staged"
    chmod 755 "$staged"
    mv -f "$staged" "$install_dir/yui"
    staged=

    printf 'Installed to %s/yui\n' "$install_dir"
    case ":${PATH:-}:" in
        *":$install_dir:"*) ;;
        *)
            printf '\nAdd ~/.local/bin to your shell PATH, then run yui:\n'
            printf '  sh/bash/zsh: export PATH="$HOME/.local/bin:$PATH"\n'
            printf '  fish: fish_add_path "$HOME/.local/bin"\n'
            ;;
    esac
}

# Keep pipe-to-sh execution from starting until the entire script is received.
main
