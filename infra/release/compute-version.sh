#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=infra/release/lib.sh
. "$script_dir/lib.sh"

channel=dev
bump=auto
explicit_version=
while (($#)); do
    case $1 in
        --channel) channel=${2:-}; shift 2 ;;
        --bump) bump=${2:-}; shift 2 ;;
        --version) explicit_version=${2:-}; shift 2 ;;
        *) release_die "usage: $0 [--channel stable|prerelease|dev] [--bump auto|patch|minor|major] [--version X.Y.Z]" 2 ;;
    esac
done
case $channel in stable|prerelease|dev) ;; *) release_die "invalid channel: $channel" 2 ;; esac
case $bump in auto|patch|minor|major) ;; *) release_die "invalid bump: $bump" 2 ;; esac
if [[ -n "$explicit_version" && $channel != stable ]]; then release_die "--version is only valid for stable releases" 2; fi
if [[ -n "$explicit_version" && ! "$explicit_version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then release_die "invalid version: $explicit_version" 2; fi

repo_root=$(git rev-parse --show-toplevel) || release_die "not inside a git repository"
if [[ $(git rev-parse --is-shallow-repository) == true ]]; then release_die "shallow repositories are unsupported" 2; fi
last_stable=
for tag in $(git tag --merged HEAD); do
    if [[ $tag =~ $OPENPUSH_STABLE_TAG_REGEX ]] && { [[ -z "$last_stable" ]] || [[ $(semver_compare "$tag" "$last_stable") == 1 ]]; }; then last_stable=$tag; fi
done
if [[ -n "$last_stable" ]]; then commits_since=$(git rev-list --count "$last_stable..HEAD"); else commits_since=$(git rev-list --count HEAD); fi
head_released=false
for tag in $(git tag --points-at HEAD); do
    if [[ $tag =~ $OPENPUSH_STABLE_TAG_REGEX ]]; then head_released=true; break; fi
done
if [[ $head_released == true && $channel == stable ]]; then release_die "HEAD already carries a stable release tag" 4; fi
if [[ -n "$explicit_version" ]] && git rev-parse -q --verify "refs/tags/v$explicit_version" >/dev/null; then
    release_die "tag v$explicit_version already exists" 2
fi

if [[ -z "$last_stable" ]]; then
    base_version=0.1.0
    resolved_bump=initial
    if [[ $bump != auto ]]; then printf '%s\n' "ignoring --bump without a stable tag" >&2; fi
elif [[ -n "$explicit_version" ]]; then
    if [[ $(semver_compare "$explicit_version" "$last_stable") != 1 ]]; then release_die "explicit version must exceed $last_stable" 2; fi
    base_version=$explicit_version
    resolved_bump=explicit
elif [[ $bump == auto ]]; then
    command -v git-cliff >/dev/null || release_die "git-cliff is required for automatic bumps"
    cliff_version=$(git-cliff --version | awk '{print $2}')
    if [[ $cliff_version != 2.14.2 ]]; then printf '%s\n' "warning: git-cliff 2.14.2 is expected (found $cliff_version)" >&2; fi
    base_version=$(cd "$repo_root" && git-cliff --config "$script_dir/cliff.toml" --bumped-version)
    base_version=${base_version#v}
    if [[ -z $(git log --no-merges -E --grep='BREAKING CHANGE:' --format=%H "$last_stable..HEAD") ]]; then
        only_non_releasable=true
        # keep in sync with infra/release/cliff.toml
        non_releasable_regex='^((docs|ci|build|refactor|test|chore|style)(\([^)]*\))?:|Merge (pull request|branch|remote-tracking branch) )'
        while IFS= read -r subject; do
            if [[ ! $subject =~ $non_releasable_regex ]]; then
                only_non_releasable=false
                break
            fi
        done < <(git log --no-merges --format=%s "$last_stable..HEAD")
        if [[ $only_non_releasable == true ]]; then base_version=${last_stable#v}; fi
    fi
    if [[ $base_version == "${last_stable#v}" ]]; then
        if [[ $channel == stable ]]; then release_die "no releasable commits since $last_stable; pass --bump or --version" 3; fi
        base_version=$(semver_bump "$last_stable" patch)
        resolved_bump=none-patch
    else
        IFS=. read -r old_major old_minor old_patch <<EOF
${last_stable#v}
EOF
        IFS=. read -r new_major new_minor new_patch <<EOF
$base_version
EOF
        if [[ $new_major != "$old_major" ]]; then
            resolved_bump=major
        elif [[ $new_minor != "$old_minor" ]]; then
            resolved_bump=minor
        elif [[ $new_patch != "$old_patch" ]]; then
            resolved_bump='patch'
        else
            resolved_bump='patch'
        fi
    fi
else
    base_version=$(semver_bump "$last_stable" "$bump")
    resolved_bump=$bump
fi
if [[ -z "$last_stable" && -n "$explicit_version" ]]; then
    base_version=$explicit_version
    resolved_bump=explicit
fi

if [[ $head_released == true ]]; then
    base_version=$(semver_bump "$last_stable" patch)
    commits_since=0
fi
case $channel in
    stable) version=$base_version; sequence=999 ;;
    prerelease) version=$base_version-main.$commits_since; sequence=$commits_since ;;
    dev) version=$base_version-dev.$commits_since; sequence=$commits_since ;;
esac
IFS=. read -r major minor patch <<EOF
$base_version
EOF
if ((10#$major > 209 || 10#$minor > 99 || 10#$patch > 99)) || { [[ $channel != stable ]] && ((10#$sequence > 998)); }; then release_die "version cannot be represented by Android/WiX" 5; fi
if ((10#$major > 255)); then release_die "version cannot be represented by WiX" 5; fi
android_version_code=$((10#$major * 10000000 + 10#$minor * 100000 + 10#$patch * 1000 + 10#$sequence))
printf 'version=%s\ntag=v%s\nbase_version=%s\nchannel=%s\nbump=%s\nlast_stable=%s\ncommits_since=%s\nhead_released=%s\nandroid_version_code=%s\nwix_version=%s.%s.%s.%s\n' "$version" "$version" "$base_version" "$channel" "$resolved_bump" "$last_stable" "$commits_since" "$head_released" "$android_version_code" "$major" "$minor" "$patch" "$sequence"
