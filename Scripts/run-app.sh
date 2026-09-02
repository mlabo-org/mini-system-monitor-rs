#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

fail() {
    printf 'run-app: %s\n' "$*" >&2
    printf '%s\n' 'Run Scripts/materialize-app.sh from the repository root, then retry.' >&2
    exit 1
}

[[ "$#" -eq 0 ]] || fail "this launcher does not accept arguments"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd "$script_dir/.." && pwd -P)"
app="$repo_root/dist/Mini System Monitor.app"
executable="$app/Contents/MacOS/mini-system-monitor-rs"

[[ -d "$app" ]] || fail "materialized app is missing: $app"
[[ -f "$executable" ]] || fail "materialized executable is missing: $executable"
[[ -x "$executable" ]] || fail "materialized executable is not executable: $executable"
for tool in codesign open; do
    command -v "$tool" >/dev/null 2>&1 || fail "required tool is unavailable: $tool"
done

codesign --verify --deep "$app" \
    || fail "materialized app failed signature verification: $app"

exec open -n "$app"
