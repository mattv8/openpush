#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: bash infra/dev/desktop.sh <dev|build|open>

On macOS, builds development .app and .dmg bundles without Developer ID signing
or notarization. Set PEPPY_MACOS_SIGNING_IDENTITY to a stable identity for
consistent Keychain access; unset uses ad-hoc signing. From WSL, runs the native
Windows Tauri toolchain against this same NTFS checkout. Linux native desktop
builds are not provided by this helper.
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

select_macos_pinned_tools() {
  local expected_node expected_rust brew prefix
  expected_node=$(tr -d '[:space:]' < "$root/.node-version")
  expected_rust=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$root/rust-toolchain.toml")

  if ! command -v node >/dev/null 2>&1 || [[ "$(node --version 2>/dev/null)" != "v$expected_node" ]]; then
    if command -v brew >/dev/null 2>&1; then
      brew=$(command -v brew)
    elif [[ -x /opt/homebrew/bin/brew ]]; then
      brew=/opt/homebrew/bin/brew
    elif [[ -x /usr/local/bin/brew ]]; then
      brew=/usr/local/bin/brew
    else
      brew=
    fi
    if [[ -n "$brew" ]] && prefix=$("$brew" --prefix node@24 2>/dev/null) && [[ -d "$prefix/bin" ]]; then
      PATH="$prefix/bin:$PATH"
      export PATH
    fi
  fi

  if ! command -v rustc >/dev/null 2>&1 || [[ "$(rustc --version 2>/dev/null)" != "rustc $expected_rust "* ]]; then
    if [[ -z ${brew:-} ]]; then
      if command -v brew >/dev/null 2>&1; then
        brew=$(command -v brew)
      elif [[ -x /opt/homebrew/bin/brew ]]; then
        brew=/opt/homebrew/bin/brew
      elif [[ -x /usr/local/bin/brew ]]; then
        brew=/usr/local/bin/brew
      fi
    fi
    if [[ -n ${brew:-} ]] && prefix=$("$brew" --prefix rustup 2>/dev/null) && [[ -d "$prefix/bin" ]]; then
      PATH="$prefix/bin:$PATH"
      export PATH
    fi
  fi
}

verify_macos_signing_identity() {
  local identity="$1"
  if [[ -z "$identity" ]]; then
    die "PEPPY_MACOS_SIGNING_IDENTITY is set but empty. Use a stable Apple Development or Developer ID identity, or unset the variable for ad-hoc signing."
  fi
  if [[ "$identity" == "-" ]]; then
    die "Ad-hoc signing (-) does not provide a stable identity. Unset PEPPY_MACOS_SIGNING_IDENTITY or choose an Apple Development (Xcode + Apple ID) or Developer ID identity."
  fi
  # Exact full name ("...") or SHA-1 hash column; partial names would be ambiguous.
  if ! security find-identity -v -p codesigning 2>/dev/null | grep -Fq -e "\"$identity\"" -e ") $identity \""; then
    die "Code signing identity not found: $identity. Run 'security find-identity -v -p codesigning' to list available identities. Install Xcode and sign in with Apple ID to create an Apple Development certificate."
  fi
}

macos() {
  local app_bundle="$target_dir/release/bundle/macos/Peppy.app"
  case "$action" in
    open)
      [[ -d "$app_bundle" ]] || die "No current macOS bundle at $app_bundle; run desktop-bundle first."
      open -n "$app_bundle"
      ;;
    dev)
      (cd "$root" && select_macos_pinned_tools && check_native_pins && pnpm install --frozen-lockfile && CARGO_TARGET_DIR="$target_dir" pnpm --dir apps/desktop exec tauri dev -- --locked)
      ;;
    build)
      if [[ ${PEPPY_MACOS_SIGNING_IDENTITY+set} == set ]]; then
        verify_macos_signing_identity "$PEPPY_MACOS_SIGNING_IDENTITY"
        (cd "$root" && select_macos_pinned_tools && check_native_pins && pnpm install --frozen-lockfile && CARGO_TARGET_DIR="$target_dir" APPLE_SIGNING_IDENTITY="$PEPPY_MACOS_SIGNING_IDENTITY" pnpm --dir apps/desktop exec tauri build --bundles app,dmg -- --locked)
      else
        (cd "$root" && select_macos_pinned_tools && check_native_pins && pnpm install --frozen-lockfile && CARGO_TARGET_DIR="$target_dir" pnpm --dir apps/desktop exec tauri build --bundles app,dmg -- --locked)
      fi
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
