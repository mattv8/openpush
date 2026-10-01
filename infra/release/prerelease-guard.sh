#!/usr/bin/env bash
set -euo pipefail
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=infra/release/lib.sh
# shellcheck disable=SC1091
source "${OPENPUSH_RELEASE_LIB:-$script_dir/lib.sh}"
from_stdin=false
if [ "${1:-}" = --tags-from-stdin ]; then from_stdin=true; shift; fi
[ "$#" -eq 1 ] || release_die "usage: $0 [--tags-from-stdin] <version>" 2
version=$1
if ! printf '%s\n' "v$version" | grep -Eq "$OPENPUSH_STABLE_TAG_REGEX|$OPENPUSH_PRERELEASE_TAG_REGEX"; then release_die "invalid version: $version" 2; fi
if "$from_stdin"; then tags=$(cat); else tags=$(git tag --list); fi
while IFS= read -r tag; do
  [ -n "$tag" ] || continue
  if printf '%s\n' "$tag" | grep -Eq "$OPENPUSH_STABLE_TAG_REGEX|$OPENPUSH_PRERELEASE_TAG_REGEX"; then
    comparison=$(semver_compare "$tag" "$version") || release_die "could not compare tag: $tag"
    if [ "$comparison" -ge 0 ]; then printf 'skip=true\n'; printf 'existing tag %s is not older than %s\n' "$tag" "$version" >&2; exit 0; fi
  fi
done <<EOF_TAGS
$tags
EOF_TAGS
printf 'skip=false\n'
