#!/usr/bin/env bash
set -euo pipefail

usage() { echo "Usage: run {build|test}" >&2; }
command=${1:-}
case "$command" in
    build|test) ;;
    *) usage; exit 64 ;;
esac

source_root=${OPENPUSH_SOURCE_ROOT:-/source}
workspace=${OPENPUSH_WORKSPACE:-/workspace}
exclude_file="$source_root/infra/dev/source-excludes.txt"
test -d "$source_root" || { echo "run: read-only source mount is missing: $source_root" >&2; exit 1; }
test -f "$exclude_file" || { echo "run: source exclusion list is missing: $exclude_file" >&2; exit 1; }
mkdir -p "$workspace"

# The source mount is deliberately read-only.  Keep generated bindings, Gradle state and Rust
# outputs in the private workspace; rsync's exclusions avoid copying host build products.
rsync -a --delete --exclude-from="$exclude_file" "$source_root/" "$workspace/"
cd "$workspace"
export OPENPUSH_ANDROID_CONTAINER=1
exec bash infra/dev/android.sh "$command"
