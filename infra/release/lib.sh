#!/usr/bin/env bash

# shellcheck disable=SC2034
OPENPUSH_STABLE_TAG_REGEX='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
# shellcheck disable=SC2034
OPENPUSH_PRERELEASE_TAG_REGEX='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-main\.(0|[1-9][0-9]*)$'

semver_compare() {
    local a b a_major a_minor a_patch a_word a_number b_major b_minor b_patch b_word b_number
    a=${1#v}
    b=${2#v}
    [[ "$a" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)(-([[:alnum:]_]+)\.([0-9]+))?$ ]] || return 2
    a_major=${BASH_REMATCH[1]}; a_minor=${BASH_REMATCH[2]}; a_patch=${BASH_REMATCH[3]}; a_word=${BASH_REMATCH[5]:-}; a_number=${BASH_REMATCH[6]:-}
    [[ "$b" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)(-([[:alnum:]_]+)\.([0-9]+))?$ ]] || return 2
    b_major=${BASH_REMATCH[1]}; b_minor=${BASH_REMATCH[2]}; b_patch=${BASH_REMATCH[3]}; b_word=${BASH_REMATCH[5]:-}; b_number=${BASH_REMATCH[6]:-}
    if ((10#$a_major < 10#$b_major)); then echo -1; return; fi
    if ((10#$a_major > 10#$b_major)); then echo 1; return; fi
    if ((10#$a_minor < 10#$b_minor)); then echo -1; return; fi
    if ((10#$a_minor > 10#$b_minor)); then echo 1; return; fi
    if ((10#$a_patch < 10#$b_patch)); then echo -1; return; fi
    if ((10#$a_patch > 10#$b_patch)); then echo 1; return; fi
    if [[ -z "$a_word" && -n "$b_word" ]]; then echo 1; return; fi
    if [[ -n "$a_word" && -z "$b_word" ]]; then echo -1; return; fi
    if [[ -z "$a_word" ]]; then echo 0; return; fi
    if [[ "$a_word" < "$b_word" ]]; then echo -1; return; fi
    if [[ "$a_word" > "$b_word" ]]; then echo 1; return; fi
    if ((10#$a_number < 10#$b_number)); then echo -1; elif ((10#$a_number > 10#$b_number)); then echo 1; else echo 0; fi
}

semver_bump() {
    local version major minor patch
    version=${1#v}
    case $2 in patch|minor|major) ;; *) return 2 ;; esac
    if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then return 2; fi
    major=${BASH_REMATCH[1]}; minor=${BASH_REMATCH[2]}; patch=${BASH_REMATCH[3]}
    case $2 in
        patch) patch=$((patch + 1)) ;;
        minor) minor=$((minor + 1)); patch=0 ;;
        major) major=$((major + 1)); minor=0; patch=0 ;;
    esac
    printf '%s.%s.%s\n' "$major" "$minor" "$patch"
}

release_die() {
    printf '%s\n' "$1" >&2
    exit "${2:-1}"
}
