#!/usr/bin/env bash
set -euo pipefail
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=infra/release/lib.sh
# shellcheck disable=SC1091
source "${OPENPUSH_RELEASE_LIB:-$script_dir/lib.sh}"
[ "$#" -eq 4 ] || release_die "usage: $0 <version> <channel> <download-dir> <out-dir>" 2
version=$1; channel=$2; download_dir=$3; out_dir=$4
case "$channel" in stable|prerelease) ;; *) release_die "invalid channel: $channel" 2 ;; esac
[ -d "$download_dir" ] || release_die "download directory not found: $download_dir" 2
find_exact() {
  matches=$(find "$1" -type f -name "$2" -print 2>/dev/null || true)
  count=$(printf '%s\n' "$matches" | sed '/^$/d' | wc -l | tr -d ' ')
  [ "$count" -eq 1 ] || release_die "expected exactly one $2 under $1; found $count"
  printf '%s\n' "$matches"
}
copy_asset() { cp "$1" "$out_dir/$2"; }
mkdir -p "$out_dir"; rm -f "$out_dir"/*
android="$download_dir/android-release-artifacts"; linux="$download_dir/desktop-linux-development-artifacts"; macos="$download_dir/desktop-macos-development-artifacts"; windows="$download_dir/desktop-windows-development-artifacts"
copy_asset "$(find_exact "$android" 'openpush-android-unsigned.apk')" "openpush-$version-android-unsigned.apk"
copy_asset "$(find_exact "$android" 'openpush-android-unsigned.aab')" "openpush-$version-android-unsigned.aab"
work_dir=$(mktemp -d); trap 'rm -rf "$work_dir"' EXIT
tar -xzf "$(find_exact "$linux" 'desktop-linux-bundles.tar.gz')" -C "$work_dir"
copy_asset "$(find_exact "$work_dir" '*.AppImage')" "openpush-$version-linux-x86_64.AppImage"
copy_asset "$(find_exact "$work_dir" '*.deb')" "openpush-$version-linux-amd64.deb"
if [ "$channel" = stable ]; then copy_asset "$(find_exact "$work_dir" '*.rpm')" "openpush-$version-linux-x86_64.rpm"; fi
copy_asset "$(find_exact "$macos" '*.dmg')" "openpush-$version-macos-aarch64.dmg"
copy_asset "$(find_exact "$macos" 'desktop-macos-bundles.tar.gz')" "openpush-$version-macos-aarch64-app.tar.gz"
copy_asset "$(find_exact "$windows" '*.msi')" "openpush-$version-windows-x64.msi"
copy_asset "$(find_exact "$windows" '*-setup.exe')" "openpush-$version-windows-x64-setup.exe"
(cd "$out_dir"; sums=SHA256SUMS.tmp; if command -v sha256sum >/dev/null 2>&1; then find . -maxdepth 1 -type f ! -name SHA256SUMS ! -name SHA256SUMS.tmp -exec sha256sum {} \; | sed 's#  \./#  #' | LC_ALL=C sort > "$sums"; else find . -maxdepth 1 -type f ! -name SHA256SUMS ! -name SHA256SUMS.tmp -exec shasum -a 256 {} \; | sed 's#  \./#  #' | LC_ALL=C sort > "$sums"; fi; mv "$sums" SHA256SUMS)
