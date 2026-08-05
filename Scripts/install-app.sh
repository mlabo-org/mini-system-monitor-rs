#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

usage() {
    printf '%s\n' \
        'Usage: Scripts/install-app.sh --destination PATH [--replace]' \
        '' \
        'Builds and installs Mini System Monitor.app at the exact destination.' \
        'Without --replace, an existing destination is never changed.' \
        'With --replace, the previous app is preserved beside the destination.'
}

fail() {
    printf 'install-app: %s\n' "$*" >&2
    exit 1
}

destination=""
replace=false
while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --destination)
            [[ "$#" -ge 2 ]] || fail "--destination requires a path"
            destination="$2"
            shift 2
            ;;
        --replace)
            replace=true
            shift
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

[[ -n "$destination" ]] || fail "--destination is required"
[[ "$destination" == */"Mini System Monitor.app" ]] \
    || fail "destination must end with: Mini System Monitor.app"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd "$script_dir/.." && pwd -P)"
destination_parent="$(dirname "$destination")"
destination_name="$(basename "$destination")"
[[ -d "$destination_parent" ]] || fail "destination parent does not exist: $destination_parent"
destination_parent="$(cd "$destination_parent" && pwd -P)"
destination="$destination_parent/$destination_name"

for tool in cargo codesign date mktemp mv; do
    command -v "$tool" >/dev/null 2>&1 || fail "required tool is unavailable: $tool"
done

if [[ -L "$destination" ]]; then
    fail "destination is a symbolic link and will not be replaced: $destination"
fi
if [[ -e "$destination" && ! -d "$destination" ]]; then
    fail "destination exists but is not an app directory: $destination"
fi
if [[ -e "$destination" && "$replace" != true ]]; then
    fail "destination already exists; use --replace only after authorizing replacement: $destination"
fi

if [[ -e "$destination" ]] && command -v pgrep >/dev/null 2>&1 && command -v ps >/dev/null 2>&1; then
    destination_executable="$destination/Contents/MacOS/mini-system-monitor-rs"
    while IFS= read -r pid; do
        command_line="$(ps -p "$pid" -o command= 2>/dev/null || true)"
        if [[ "$command_line" == "$destination_executable"* ]]; then
            fail "the destination app is running; quit it before replacement: $destination"
        fi
    done < <(pgrep -x mini-system-monitor-rs || true)
fi

stage_dir="$(mktemp -d /private/tmp/mini-system-monitor-install.XXXXXX)"
previous_app=""
cleanup() {
    status=$?
    if [[ "$status" -ne 0 && -n "$previous_app" && -e "$previous_app" && ! -e "$destination" ]]; then
        mv "$previous_app" "$destination" || true
    fi
    if [[ -d "$stage_dir" ]]; then
        rm -rf -- "$stage_dir"
    fi
    exit "$status"
}
trap cleanup EXIT INT HUP TERM

"$repo_root/Scripts/build-app.sh" --output "$stage_dir/output"
built_app="$stage_dir/output/Mini System Monitor.app"
[[ -d "$built_app" ]] || fail "built app is missing: $built_app"

if [[ -e "$destination" ]]; then
    timestamp="$(date +%Y%m%d-%H%M%S)"
    previous_app="$destination_parent/Mini System Monitor.previous-$timestamp-$$.app"
    [[ ! -e "$previous_app" ]] || fail "backup destination already exists: $previous_app"
    mv "$destination" "$previous_app"
fi

mv -n "$built_app" "$destination"
[[ -d "$destination" && ! -e "$built_app" ]] || fail "installation did not complete: $destination"
if ! codesign --verify --deep --strict "$destination"; then
    mv "$destination" "$stage_dir/failed-install.app"
    if [[ -n "$previous_app" && -e "$previous_app" ]]; then
        mv "$previous_app" "$destination"
        previous_app=""
    fi
    fail "installed app failed signature verification"
fi

printf 'Installed and verified: %s\n' "$destination"
if [[ -n "$previous_app" ]]; then
    printf 'Previous app preserved: %s\n' "$previous_app"
fi

trap - EXIT INT HUP TERM
rm -rf -- "$stage_dir"
