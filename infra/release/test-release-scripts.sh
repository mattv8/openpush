#!/usr/bin/env bash
set -euo pipefail
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=infra/release/lib.sh
# shellcheck disable=SC1091
source "${OPENPUSH_RELEASE_LIB:-$script_dir/lib.sh}"
root=$(mktemp -d); trap 'rm -rf "$root"' EXIT
fixture="$root/download"
mkdir -p "$fixture/android-release-artifacts" "$fixture/desktop-linux-development-artifacts" "$fixture/desktop-macos-development-artifacts/dist" "$fixture/desktop-macos-development-artifacts/apps/desktop/src-tauri/target/release/bundle/dmg" "$fixture/desktop-windows-development-artifacts/nsis" "$fixture/desktop-windows-development-artifacts/msi" "$fixture/ios-development-artifacts"
printf release > "$fixture/android-release-artifacts/openpush-android-unsigned.apk"
printf bundle > "$fixture/android-release-artifacts/openpush-android-unsigned.aab"
mkdir -p "$root/linux/appimage" "$root/linux/deb" "$root/linux/rpm"
printf appimage > "$root/linux/appimage/OpenPush.AppImage"; printf deb > "$root/linux/deb/openpush.deb"; printf rpm > "$root/linux/rpm/openpush.rpm"
tar -C "$root/linux" -czf "$fixture/desktop-linux-development-artifacts/desktop-linux-bundles.tar.gz" appimage deb rpm
printf mac > "$fixture/desktop-macos-development-artifacts/apps/desktop/src-tauri/target/release/bundle/dmg/OpenPush_X_aarch64.dmg"
printf archive > "$fixture/desktop-macos-development-artifacts/dist/desktop-macos-bundles.tar.gz"
printf exe > "$fixture/desktop-windows-development-artifacts/nsis/OpenPush_X_x64-setup.exe"; printf msi > "$fixture/desktop-windows-development-artifacts/msi/OpenPush_X_x64_en-US.msi"
"$script_dir/stage-assets.sh" 0.2.0-main.5 prerelease "$fixture" "$root/prerelease"
test -f "$root/prerelease/openpush-0.2.0-main.5-linux-amd64.deb"; test ! -e "$root/prerelease/openpush-0.2.0-main.5-linux-x86_64.rpm"; test -s "$root/prerelease/SHA256SUMS"
test -f "$root/prerelease/openpush-0.2.0-main.5-android-unsigned.apk"; test -f "$root/prerelease/openpush-0.2.0-main.5-android-unsigned.aab"
test ! -e "$root/prerelease/openpush-0.2.0-main.5-android-debug.apk"
test "$(find "$root/prerelease" -type f | wc -l | tr -d ' ')" = 9; test "$(grep -c . "$root/prerelease/SHA256SUMS")" = 8
grep -Fq 'openpush-0.2.0-main.5-android-unsigned.apk' "$root/prerelease/SHA256SUMS"; grep -Fq 'openpush-0.2.0-main.5-android-unsigned.aab' "$root/prerelease/SHA256SUMS"
"$script_dir/stage-assets.sh" 0.2.0 stable "$fixture" "$root/stable"; test -f "$root/stable/openpush-0.2.0-linux-x86_64.rpm"
rm "$fixture/android-release-artifacts/openpush-android-unsigned.apk"
if "$script_dir/stage-assets.sh" 0.2.0 stable "$fixture" "$root/missing" >/dev/null 2>&1; then printf 'expected missing asset failure\n' >&2; exit 1; fi
printf release > "$fixture/android-release-artifacts/openpush-android-unsigned.apk"; mkdir "$fixture/android-release-artifacts/duplicate"; cp "$fixture/android-release-artifacts/openpush-android-unsigned.apk" "$fixture/android-release-artifacts/duplicate/openpush-android-unsigned.apk"
if "$script_dir/stage-assets.sh" 0.2.0 stable "$fixture" "$root/duplicate" >/dev/null 2>&1; then printf 'expected duplicate asset failure\n' >&2; exit 1; fi
rm -rf "$fixture/android-release-artifacts/duplicate"
selected=$(printf '%s\n' '' v0.2.0-main.9 v0.2.0-main.10 v0.1.0-main.99 v0.2.0 other | "$script_dir/prune-prereleases.sh" --keep 2 --select)
test "$selected" = v0.1.0-main.99
test "$(printf '%s\n' 'v0.2.0-main.6' | "$script_dir/prune-prereleases.sh" --keep 10 --select)" = ''
check_guard() { expected=$1; tags=$2; actual=$(printf '%s\n' "$tags" | "$script_dir/prerelease-guard.sh" --tags-from-stdin 0.2.0-main.5); test "$actual" = "skip=$expected"; }
check_guard true v0.2.0-main.6; check_guard true v0.2.0-main.5; check_guard true v0.2.0; check_guard false v0.1.9-main.99; check_guard false ''
printf 'release script tests passed\n'
