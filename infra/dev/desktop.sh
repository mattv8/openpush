#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: bash infra/dev/desktop.sh <dev|build|open>

On macOS, builds development .app and .dmg bundles without Developer ID signing
or notarization. From WSL, runs
the native Windows Tauri toolchain against this same NTFS checkout. Linux
native desktop builds are not provided by this helper.
EOF
  exit 64
}

die() {
  printf 'desktop: %s\n' "$*" >&2
  exit 1
}

action=${1:-}
[[ $# -eq 1 ]] || usage
case "$action" in dev|build|open) ;; *) usage ;; esac

script_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
root=$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)

target_dir=${CARGO_TARGET_DIR:-"$root/apps/desktop/src-tauri/target"}
if [[ "$target_dir" != /* ]]; then
  target_dir="$root/$target_dir"
fi

check_native_pins() {
  local expected_node expected_rust node_version pnpm_version rust_version
  expected_node=$(tr -d '[:space:]' < "$root/.node-version")
  expected_rust=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$root/rust-toolchain.toml")
  command -v node >/dev/null || die "Node $expected_node is required."
  command -v pnpm >/dev/null || die "pnpm 12.8.1 is required."
  command -v rustc >/dev/null || die "Rust $expected_rust is required."
  node_version=$(node --version)
  pnpm_version=$(pnpm --version)
  rust_version=$(rustc --version)
  [[ "$node_version" == "v$expected_node" ]] || die "Node $expected_node is required (found $node_version)."
  [[ "$pnpm_version" == "12.8.1" ]] || die "pnpm 12.8.1 is required (found $pnpm_version)."
  [[ "$rust_version" == "rustc $expected_rust "* ]] || die "Rust $expected_rust is required (found $rust_version)."
}

macos() {
  local app_bundle="$target_dir/release/bundle/macos/OpenPush.app"
  case "$action" in
    open)
      [[ -d "$app_bundle" ]] || die "No current macOS bundle at $app_bundle; run desktop-bundle first."
      open "$app_bundle"
      ;;
    dev)
      (cd "$root" && check_native_pins && pnpm install --frozen-lockfile)
      (cd "$root" && CARGO_TARGET_DIR="$target_dir" pnpm --dir apps/desktop exec tauri dev -- --locked)
      ;;
    build)
      (cd "$root" && check_native_pins && pnpm install --frozen-lockfile)
      (cd "$root" && CARGO_TARGET_DIR="$target_dir" pnpm --dir apps/desktop exec tauri build --bundles app,dmg -- --locked)
      [[ -d "$app_bundle" ]] || die "Tauri completed without the expected bundle: $app_bundle"
      ;;
  esac
}

wsl_windows() {
  local root_windows target_windows powershell
  command -v wslpath >/dev/null || die "WSL interop is unavailable; run this from WSL backed by a Windows NTFS drive."
  root_windows=$(wslpath -w "$root")
  [[ "$root_windows" =~ ^[A-Za-z]:\\ ]] || die "Native Windows builds require this checkout on a drive-letter NTFS path; ext4 and UNC paths are unsupported."
  target_windows=$(wslpath -w "$target_dir")
  [[ "$target_windows" =~ ^[A-Za-z]:\\ ]] || die "CARGO_TARGET_DIR must resolve to a drive-letter Windows path."
  if command -v powershell.exe >/dev/null; then
    powershell=$(command -v powershell.exe)
  elif command -v pwsh.exe >/dev/null; then
    powershell=$(command -v pwsh.exe)
  else
    die "Windows PowerShell was not found through WSL interop."
  fi
  "$powershell" -NoProfile -ExecutionPolicy Bypass -File "${root_windows}\\infra\\dev\\windows-desktop.ps1" \
    -Action "$action" -RepoPath "$root_windows" -CargoTargetDir "$target_windows"
}

case "$(uname -s)" in
  Darwin) macos ;;
  Linux)
    if grep -qi microsoft /proc/sys/kernel/osrelease 2>/dev/null || [[ -n ${WSL_INTEROP:-} ]]; then
      wsl_windows
    else
      die "Native desktop development is supported on macOS or from WSL using the Windows toolchain; Linux packaging remains CI-only."
    fi
    ;;
  *) die "Unsupported host; use macOS or WSL with Windows interop." ;;
esac
