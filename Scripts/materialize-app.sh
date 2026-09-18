#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

usage() {
    printf '%s\n' \
        'Usage: Scripts/materialize-app.sh' \
        '' \
        'Builds, signs, and atomically materializes the normal local runtime at:' \
        '  dist/Mini System Monitor.app'
}

fail() {
    printf 'materialize-app: %s\n' "$*" >&2
    exit 1
}

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --help)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd "$script_dir/.." && pwd -P)"
artifact_root="$repo_root/dist"
destination="$artifact_root/Mini System Monitor.app"

for tool in codesign mkdir mktemp mv xattr; do
    command -v "$tool" >/dev/null 2>&1 || fail "required tool is unavailable: $tool"
done

[[ ! -L "$artifact_root" ]] || fail "artifact root is a symbolic link: $artifact_root"
[[ ! -L "$destination" ]] || fail "destination is a symbolic link: $destination"
if [[ -e "$destination" && ! -d "$destination" ]]; then
    fail "destination exists but is not an app directory: $destination"
fi

stage_dir="$(mktemp -d /private/tmp/mini-system-monitor-materialize.XXXXXX)"
previous_app=""
remove_stage() {
    find "$stage_dir" -type f -delete
    find "$stage_dir" -depth -type d -exec rmdir {} \;
}
cleanup() {
    status=$?
    if [[ "$status" -ne 0 && -n "$previous_app" && -e "$previous_app" && ! -e "$destination" ]]; then
        mv "$previous_app" "$destination" || true
    fi
    if [[ -d "$stage_dir" ]]; then
        remove_stage
    fi
    exit "$status"
}
trap cleanup EXIT INT HUP TERM

"$repo_root/Scripts/build-app.sh" --output "$stage_dir/output"
built_app="$stage_dir/output/Mini System Monitor.app"
[[ -d "$built_app" ]] || fail "built app is missing: $built_app"

mkdir -p "$artifact_root"
if [[ -e "$destination" ]]; then
    previous_app="$stage_dir/previous.app"
    mv "$destination" "$previous_app"
fi

mv -n "$built_app" "$destination"
[[ -d "$destination" && ! -e "$built_app" ]] \
    || fail "materialization did not complete: $destination"
xattr -cr "$destination"
codesign --force --deep --sign - "$destination"
if ! codesign --verify --deep --strict "$destination"; then
    mv "$destination" "$stage_dir/failed-materialization.app"
    fail "materialized app failed signature verification"
fi

printf 'Materialized and verified: %s\n' "$destination"
printf 'Run directly: open %q\n' "$destination"

trap - EXIT INT HUP TERM
remove_stage
