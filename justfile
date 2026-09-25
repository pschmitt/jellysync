# `just` is the entry point for development tasks. Anything that compiles Rust or
# builds with Nix runs on a remote build host (rofl-13 by default) in a throwaway
# directory under ~/build/jellysync.

set shell := ["bash", "-euo", "pipefail", "-c"]

export CARGO_TERM_COLOR := "always"

default-host := "rofl-13"
remote-build-base := "~/build/jellysync"

# Running just without arguments lists the available recipes.
[private]
default:
    @just -l

# Build a debug binary remotely and fetch it to target/debug/jellysync.
build host=default-host:
    just _remote "{{ host }}" "nix develop path:. -c cargo build --locked" target/debug/jellysync

# Build a release binary remotely and fetch it to target/release/jellysync.
release host=default-host:
    just _remote "{{ host }}" "nix develop path:. -c cargo build --locked --release" target/release/jellysync

# Run the test suite remotely.
test host=default-host:
    just _remote "{{ host }}" "nix develop path:. -c cargo test --locked"

# Run cargo check remotely.
cargo-check host=default-host:
    just _remote "{{ host }}" "nix develop path:. -c cargo check --locked --all-targets"

# Run clippy remotely with warnings promoted to errors.
clippy host=default-host:
    just _remote "{{ host }}" "nix develop path:. -c cargo clippy --locked --all-targets -- -Dwarnings"

# Format Rust, justfile, and Nix sources.
format:
    nix develop -c cargo fmt --all
    just --fmt
    nix develop -c nixfmt flake.nix

# Check Rust, justfile, and Nix formatting.
format-check:
    nix develop -c cargo fmt --all -- --check
    just --fmt --check
    nix develop -c nixfmt --check flake.nix

# Lint Nix sources with statix and deadnix.
nix-lint:
    nix develop -c statix check .
    nix develop -c deadnix --fail flake.nix

# Formatting checks plus clippy and Nix linters.
lint: format-check nix-lint clippy

# Build the flake package remotely and show build logs.
nix-build host=default-host:
    just _remote "{{ host }}" "nix build --print-build-logs 'path:.#default'"

# Evaluate all flake checks remotely.
nix-check host=default-host:
    just _remote "{{ host }}" "nix flake check --print-build-logs path:."

# Run the full validation suite.
check: lint test nix-build

# Retake docs/screenshot.png on a remote host with Docker: a throwaway Jellyfin
# with Creative Commons media, the TUI in a real kitty (see scripts/screenshot.sh).
# font_dir is an optional local font directory for font_family (e.g. a non-free
# font that cannot live in this repository).
screenshot host=default-host font_family="ComicCode Nerd Font" font_dir="":
    #!/usr/bin/env bash
    set -euo pipefail
    host="{{ host }}"
    font_family="{{ font_family }}"
    font_dir="{{ font_dir }}"
    # Without a font directory, take the family from this machine's fonts
    # (e.g. a non-free font); fall back to the script's default font.
    if [[ -z "$font_dir" ]]
    then
      font_file="$(fc-list --format '%{file}\n' ":family=$font_family" 2>/dev/null | head -1)"
      if [[ -n "$font_file" ]]
      then
        font_dir="$(dirname -- "$font_file")"
      else
        echo "Font '$font_family' not found locally; using the default font" >&2
        font_family=""
      fi
    fi
    remote_dir="$(ssh -- "$host" "mkdir -p {{ remote-build-base }} && mktemp -d {{ remote-build-base }}/screenshot.XXXXXX")"
    cleanup() {
      ssh -- "$host" rm -rf -- "$remote_dir"
    }
    trap cleanup EXIT
    rsync -az --delete --exclude target --exclude result --exclude .git --exclude .screenshot ./ "$host:$remote_dir/"
    font_env=""
    if [[ -n "$font_dir" ]]
    then
      ssh -- "$host" mkdir -p "$remote_dir/.screenshot/fonts"
      # Nix store fonts are read-only; keep the copy removable.
      rsync -a --chmod=u+w "$font_dir/" "$host:$remote_dir/.screenshot/fonts/"
      font_env="FONT_DIR=$remote_dir/.screenshot/fonts"
    fi
    if [[ -n "$font_family" ]]
    then
      font_env="$font_env FONT_FAMILY='$font_family'"
    fi
    ssh -- "$host" "cd '$remote_dir' && $font_env scripts/screenshot.sh"
    rsync -az "$host:$remote_dir/docs/screenshot.png" docs/screenshot.png
    echo "Updated docs/screenshot.png"

# Remove Cargo's local build output.
clean:
    rm -rf target result

# Remove all remote build directories.
clean-remote host=default-host:
    ssh -- "{{ host }}" "rm -rf -- {{ remote-build-base }}"

# Sync the tree to a fresh remote directory, run a command there, optionally fetch
# one artifact back, and always remove the remote directory afterwards.
[private]
_remote host command fetch="":
    #!/usr/bin/env bash
    set -euo pipefail
    host="{{ host }}"
    remote_dir="$(ssh -- "$host" "mkdir -p {{ remote-build-base }} && mktemp -d {{ remote-build-base }}/run.XXXXXX")"
    cleanup() {
      ssh -- "$host" rm -rf -- "$remote_dir"
    }
    trap cleanup EXIT
    rsync -az --delete --exclude target --exclude result --exclude .git ./ "$host:$remote_dir/"
    ssh -- "$host" "cd '$remote_dir' && {{ command }}"
    if [[ -n "{{ fetch }}" ]]
    then
      mkdir -p "$(dirname "{{ fetch }}")"
      rsync -az "$host:$remote_dir/{{ fetch }}" "{{ fetch }}"
    fi
