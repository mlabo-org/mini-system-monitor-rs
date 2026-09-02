#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd "$script_dir/.." && pwd -P)"

for tool in cargo mktemp; do
    command -v "$tool" >/dev/null 2>&1 || {
        printf 'check: required tool is unavailable: %s\n' "$tool" >&2
        exit 1
    }
done

validation_root="$(mktemp -d /private/tmp/mini-system-monitor-check.XXXXXX)"
cleanup() {
    if [[ -d "$validation_root" ]]; then
        rm -rf -- "$validation_root"
    fi
}
trap cleanup EXIT INT HUP TERM

cd "$repo_root"
cargo fmt --check
CARGO_TARGET_DIR="$validation_root/cargo-target" cargo test --locked
CARGO_TARGET_DIR="$validation_root/cargo-target" \
    cargo clippy --all-targets --all-features -- -D warnings
CARGO_TARGET_DIR="$validation_root/cargo-target" cargo build --locked
