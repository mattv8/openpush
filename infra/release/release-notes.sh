#!/usr/bin/env bash
set -euo pipefail

if (($# != 1)); then echo "usage: $0 <version>" >&2; exit 2; fi
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
case $1 in *-*) kind=prerelease ;; *) kind='stable release' ;; esac
printf 'This is an unsigned development-grade %s. It is not notarized, signed, or store-distributed.\n\n' "$kind"
git cliff --config "$script_dir/cliff.toml" --unreleased --tag "v$1" --strip all
