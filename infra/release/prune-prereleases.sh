#!/usr/bin/env bash
set -euo pipefail
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=infra/release/lib.sh
# shellcheck disable=SC1091
source "${OPENPUSH_RELEASE_LIB:-$script_dir/lib.sh}"
keep=10; select_only=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    --keep) shift; [ "$#" -gt 0 ] || release_die "--keep requires a value" 2; keep=$1 ;;
    --select) select_only=true ;;
    *) release_die "usage: $0 [--keep N] [--select]" 2 ;;
  esac
  shift
done
case "$keep" in ''|*[!0-9]*) release_die "invalid keep count: $keep" 2 ;; esac
sort_tags() {
  sorted=''
  while IFS= read -r tag; do
    [ -n "$tag" ] || continue
    inserted=false; result=''
    for current in $sorted; do
      if ! "$inserted" && [ "$(semver_compare "$tag" "$current")" -lt 0 ]; then result="$result $tag"; inserted=true; fi
      result="$result $current"
    done
    if ! "$inserted"; then result="$result $tag"; fi
    sorted=$result
  done
  printf '%s\n' "$sorted" | tr ' ' '\n' | sed '/^$/d'
}
select_tags() {
  candidates=$(grep -E "$OPENPUSH_PRERELEASE_TAG_REGEX" || true)
  ordered=$(printf '%s\n' "$candidates" | sort_tags)
  total=$(printf '%s\n' "$ordered" | sed '/^$/d' | wc -l | tr -d ' ')
  delete_count=$((total - keep))
  if [ "$delete_count" -gt 0 ]; then printf '%s\n' "$ordered" | sed -n "1,${delete_count}p"; fi
}
if "$select_only"; then select_tags; exit 0; fi
releases=$(gh release list --limit 1000 --json tagName,isPrerelease,isDraft)
draft_tags=$(printf '%s\n' "$releases" | jq -r '.[] | select(.isDraft) | .tagName' | grep -E "$OPENPUSH_PRERELEASE_TAG_REGEX" || true)
published_tags=$(printf '%s\n' "$releases" | jq -r '.[] | select(.isPrerelease and (.isDraft | not)) | .tagName')
for tag in $draft_tags; do gh release delete "$tag" --yes; done
for tag in $(printf '%s\n' "$published_tags" | select_tags); do gh release delete "$tag" --cleanup-tag --yes; done
