#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

usage() {
    printf '%s\n' \
        'Usage: Scripts/build-app.sh --output PATH' \
        '' \
        'Builds and ad-hoc signs PATH/Mini System Monitor.app.' \
        'Uses a temporary Cargo target and removes it when the command exits.' \
        'The destination app must not already exist.'
}

fail() {
    printf 'build-app: %s\n' "$*" >&2
    exit 1
}

output=""
while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --output)
            [[ "$#" -ge 2 ]] || fail "--output requires a path"
            output="$2"
            shift 2
            ;;
        --help)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

[[ -n "$output" ]] || fail "--output is required"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd "$script_dir/.." && pwd -P)"
case "$output" in
    /*) ;;
    *) output="$PWD/$output" ;;
esac

destination="$output/Mini System Monitor.app"
[[ ! -e "$destination" ]] || fail "destination already exists: $destination"
[[ -f "$repo_root/App/Info.plist" ]] || fail "App/Info.plist is missing"
[[ -f "$repo_root/App/AppIcon.icns" ]] || fail "App/AppIcon.icns is missing"

for tool in cargo plutil codesign mktemp cp mv mkdir chmod; do
    command -v "$tool" >/dev/null 2>&1 || fail "required tool is unavailable: $tool"
done

work_dir="$(mktemp -d /private/tmp/mini-system-monitor-build.XXXXXX)"
build_root="$work_dir/cargo-target"
stage_app="$work_dir/Mini System Monitor.app"
cleanup() {
    if [[ -d "$work_dir" ]]; then
        rm -rf -- "$work_dir"
    fi
}
trap cleanup EXIT INT HUP TERM

(
    cd "$repo_root"
    CARGO_TARGET_DIR="$build_root" cargo build --release --locked --offline
)

executable="$build_root/release/mini-system-monitor-rs"
[[ -x "$executable" ]] || fail "release executable is missing: $executable"

mkdir -p "$stage_app/Contents/MacOS" "$stage_app/Contents/Resources"
cp "$repo_root/App/Info.plist" "$stage_app/Contents/Info.plist"
cp "$repo_root/App/AppIcon.icns" "$stage_app/Contents/Resources/AppIcon.icns"
cp "$executable" "$stage_app/Contents/MacOS/mini-system-monitor-rs"
chmod 755 "$stage_app/Contents/MacOS/mini-system-monitor-rs"

plutil -lint "$stage_app/Contents/Info.plist" >/dev/null
[[ "$(plutil -extract CFBundleIconFile raw -o - "$stage_app/Contents/Info.plist")" == "AppIcon" ]] \
    || fail "CFBundleIconFile must be AppIcon"
codesign --force --deep --sign - "$stage_app"
codesign --verify --deep --strict "$stage_app"

mkdir -p "$output"
mv -n "$stage_app" "$destination"
printf 'Built and verified: %s\n' "$destination"
