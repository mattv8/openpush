#!/usr/bin/env bash
# Build and run the debug iOS application on one compatible simulator.
set -euo pipefail

ROOT="${PEPPY_REPOSITORY_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
cd "$ROOT"
ARTIFACTS="${PEPPY_IOS_ARTIFACTS:-$ROOT/.opencode/dev/artifacts/ios}"
SCRATCH="${PEPPY_IOS_SCRATCH:-$ROOT/.opencode/sessions/ios-simulator-actions}"
RUST_ARTIFACTS="${CARGO_TARGET_DIR:-$ROOT/target}"
[[ "$RUST_ARTIFACTS" == /* ]] || RUST_ARTIFACTS="$ROOT/$RUST_ARTIFACTS"
APP_BUNDLE_ID=dev.peppy.mobile
BUILD_SCRATCH=
SIMULATOR_FRONTEND=

fail() { echo "ios: $*" >&2; exit 1; }
require() { command -v "$1" >/dev/null || fail "$1 is required"; }
full_xcode() {
  [[ -x "$1/usr/bin/xcodebuild" ]] || return 1
  # Xcode 26 and earlier: Contents/Developer/Applications/Simulator.app
  [[ -d "$1/Applications/Simulator.app" ]] && return 0
  # Xcode 27+: DeviceHub.app in Contents/Applications
  [[ -d "$1/../Applications/DeviceHub.app" ]] && return 0
  return 1
}

prepare_rust_path() {
  # Homebrew's rustup proxies are not always on an editor terminal's PATH.
  if ! command -v cargo >/dev/null; then
    local directory
    for directory in "$HOME/.cargo/bin" /opt/homebrew/opt/rustup/bin /usr/local/opt/rustup/bin; do
      if [[ -x "$directory/cargo" ]]; then
        export PATH="$directory:$PATH"
        break
      fi
    done
  fi
}

prepare_xcode() {
  if [[ -z "${DEVELOPER_DIR:-}" ]]; then
    DEVELOPER_DIR=$(xcode-select -p 2>/dev/null || true)
    if ! full_xcode "$DEVELOPER_DIR" && full_xcode /Applications/Xcode.app/Contents/Developer; then
      DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
    fi
  fi
  full_xcode "$DEVELOPER_DIR" || fail "full Xcode with Simulator.app (Xcode 26) or Device Hub (Xcode 27+) is required; repair or install Xcode from the App Store or developer.apple.com/xcode, or set DEVELOPER_DIR to a complete Xcode Contents/Developer directory"
  export DEVELOPER_DIR
  local version major
  version=$(xcodebuild -version) || fail "xcodebuild is unavailable from DEVELOPER_DIR"
  major=$(awk '/^Xcode / { split($2, v, "."); print v[1]; exit }' <<<"$version")
  [[ "$major" =~ ^[0-9]+$ && "$major" -ge 26 ]] || fail "Xcode 26 or newer is required (found: $version)"
  # Detect which frontend is available for this Xcode
  if [[ -d "$DEVELOPER_DIR/Applications/Simulator.app" ]]; then
    SIMULATOR_FRONTEND="$DEVELOPER_DIR/Applications/Simulator.app"
  elif [[ -d "$DEVELOPER_DIR/../Applications/DeviceHub.app" ]]; then
    SIMULATOR_FRONTEND="$DEVELOPER_DIR/../Applications/DeviceHub.app"
  fi
}

preflight_tools() {
  [[ "$(uname -s)" == Darwin ]] || fail "iOS Simulator actions require macOS"
  prepare_rust_path
  for tool in xcode-select xcodebuild xcrun cargo rustup python3 open; do require "$tool"; done
  prepare_xcode
  case "$(uname -m)" in
    arm64) RUST_TARGET=aarch64-apple-ios-sim; ARCH=arm64 ;;
    x86_64) RUST_TARGET=x86_64-apple-ios; ARCH=x86_64 ;;
    *) fail "unsupported Mac architecture; arm64 or x86_64 is required" ;;
  esac
  APPLE_TARGET="$ARCH-apple-ios26.0-simulator"
  SDKROOT=$(xcrun --sdk iphonesimulator --show-sdk-path) || fail "iOS Simulator SDK is unavailable"
  [[ -r "$SDKROOT" ]] || fail "iOS Simulator SDK is unavailable"
  CLANG=$(xcrun --sdk iphonesimulator --find clang) || fail "iOS Simulator clang is unavailable"
  AR=$(xcrun --sdk iphonesimulator --find ar) || fail "iOS Simulator ar is unavailable"
  RANLIB=$(xcrun --sdk iphonesimulator --find ranlib) || fail "iOS Simulator ranlib is unavailable"
  [[ -x "$CLANG" && -x "$AR" && -x "$RANLIB" ]] || fail "iOS Simulator build tools are unavailable"
}

select_simulator() {
  local devices runtimes selection
  devices=$(xcrun simctl list devices available -j) || fail "unable to list iOS simulators"
  runtimes=$(xcrun simctl list runtimes -j) || fail "unable to list iOS runtimes"
  selection=$(IOS_DEVICES="$devices" IOS_RUNTIMES="$runtimes" IOS_OVERRIDE="${PEPPY_IOS_SIMULATOR:-}" python3 - <<'PY'
import json
import os
import re
import sys

def fail(message):
    sys.stderr.write('ios: ' + message + '\n')
    sys.exit(1)

devices = json.loads(os.environ['IOS_DEVICES']).get('devices', {})
runtimes = json.loads(os.environ['IOS_RUNTIMES']).get('runtimes', [])
available = {}
for runtime in runtimes:
    identifier = runtime.get('identifier', '')
    match = re.fullmatch(r'com\.apple\.CoreSimulator\.SimRuntime\.iOS-(\d+(?:-\d+)*)', identifier)
    if runtime.get('isAvailable') and match:
        version = tuple(int(part) for part in match.group(1).split('-'))
        if version >= (26,):
            available[identifier] = version
if not available:
    fail('No available iOS 26 or newer Simulator runtime. Install one in Xcode Settings > Components.')
candidates = []
for runtime, entries in devices.items():
    if runtime not in available:
        continue
    for device in entries:
        device_type = device.get('deviceTypeIdentifier', '')
        is_iphone = (device_type.startswith('com.apple.CoreSimulator.SimDeviceType.iPhone-')
                     if device_type else device.get('name', '').startswith('iPhone'))
        if device.get('isAvailable') and is_iphone:
            candidates.append((runtime, device))
override = os.environ.get('IOS_OVERRIDE')
if override:
    matches = [(r, d) for r, d in candidates if d.get('udid') == override or d.get('name') == override]
    if not matches:
        fail('PEPPY_IOS_SIMULATOR must name an available iPhone simulator at iOS 26 or newer.')
    if len(matches) != 1:
        fail('PEPPY_IOS_SIMULATOR is ambiguous; use an exact simulator UDID.')
    selected = matches[0]
else:
    booted = [(r, d) for r, d in candidates if d.get('state') == 'Booted']
    if len(booted) > 1:
        fail('multiple booted compatible iPhone simulators; set PEPPY_IOS_SIMULATOR to a name or UDID.')
    if booted:
        selected = booted[0]
    elif candidates:
        newest = max(available[runtime] for runtime, _ in candidates)
        selected = min((item for item in candidates if available[item[0]] == newest),
                       key=lambda item: (item[1].get('name', ''), item[1].get('udid', '')))
    else:
        fail('No available compatible iPhone simulator. Install one in Xcode Settings > Components.')
print(selected[1]['udid'])
PY
) || exit $?
  SIMULATOR_UDID=$selection
}

check() {
  preflight_tools
  select_simulator
  echo "iOS simulator selected: $SIMULATOR_UDID"
}

build() {
  mkdir -p "$ARTIFACTS" "$SCRATCH"
  BUILD_SCRATCH=$(mktemp -d "$SCRATCH/build.XXXXXX")
  trap '[[ -z "$BUILD_SCRATCH" ]] || rm -rf "$BUILD_SCRATCH"' EXIT
  rustup target add "$RUST_TARGET"
  cargo build -p peppy-mobile-bindings --locked --target-dir "$RUST_ARTIFACTS"
  local generated="$BUILD_SCRATCH/generated-swift" compiler_wrapper="$BUILD_SCRATCH/clang"
  mkdir -p "$generated"
  cargo run --locked -p peppy-mobile-bindings --features cli --bin uniffi-bindgen --target-dir "$RUST_ARTIFACTS" -- generate \
    --library "$RUST_ARTIFACTS/debug/libpeppy_mobile_bindings.dylib" --language swift --out-dir "$generated"
  if ! diff -ru --exclude=module.modulemap "$ROOT/apps/ios/Generated" "$generated"; then
    fail "Generated iOS bindings drift from Rust source; regenerate Swift bindings using the command in CONTRIBUTING.md > iOS Simulator workflow before building."
  fi
  cat > "$compiler_wrapper" <<'EOF'
#!/bin/sh
exec "$PEPPY_IOS_CLANG" "$@" -target "$PEPPY_IOS_TARGET" -isysroot "$PEPPY_IOS_SDKROOT" "$PEPPY_IOS_DEPLOYMENT_FLAG"
EOF
  chmod +x "$compiler_wrapper"
  local target_env=${RUST_TARGET//-/_} target_upper
  target_upper=$(tr '[:lower:]' '[:upper:]' <<<"$target_env")
  env SDKROOT="$SDKROOT" IPHONEOS_DEPLOYMENT_TARGET=26.0 PEPPY_IOS_CLANG="$CLANG" PEPPY_IOS_SDKROOT="$SDKROOT" \
    PEPPY_IOS_TARGET="$APPLE_TARGET" PEPPY_IOS_DEPLOYMENT_FLAG=-mios-simulator-version-min=26.0 \
    CFLAGS="-target $APPLE_TARGET -isysroot $SDKROOT -mios-simulator-version-min=26.0" \
    "CC_$target_env=$compiler_wrapper" "AR_$target_env=$AR" "RANLIB_$target_env=$RANLIB" \
    "CARGO_TARGET_${target_upper}_LINKER=$compiler_wrapper" cargo build -p peppy-mobile-bindings --locked --target "$RUST_TARGET" --target-dir "$RUST_ARTIFACTS"
  xcodebuild -project "$ROOT/apps/ios/PeppyMobile.xcodeproj" -scheme PeppyMobile -configuration Debug -sdk iphonesimulator \
    -destination "platform=iOS Simulator,id=$SIMULATOR_UDID" -derivedDataPath "$ARTIFACTS" \
    "LIBRARY_SEARCH_PATHS=\"$RUST_ARTIFACTS/$RUST_TARGET/debug\"" "ARCHS=$ARCH" ONLY_ACTIVE_ARCH=YES CODE_SIGNING_ALLOWED=YES build
  APP_PATH="$ARTIFACTS/Build/Products/Debug-iphonesimulator/PeppyMobile.app"
  [[ -d "$APP_PATH" ]] || fail "Xcode Debug build did not produce PeppyMobile.app"
}

run() {
  check
  # The existing dev-up action owns backend setup; invoke it only after preflight.
  bash "$ROOT/infra/dev/dev.sh" dev-up
  xcrun simctl bootstatus "$SIMULATOR_UDID" -b || fail "simulator boot failed; check its runtime in Xcode Settings > Components"
  # Open the detected frontend: Simulator.app (Xcode 26) or DeviceHub.app (Xcode 27+)
  if [[ "$SIMULATOR_FRONTEND" == *"Simulator.app" ]]; then
    # Legacy Simulator.app accepts -CurrentDeviceUDID preference argument
    open "$SIMULATOR_FRONTEND" --args -CurrentDeviceUDID "$SIMULATOR_UDID"
  else
    # DeviceHub.app is opened without legacy Simulator args; simctl remains pinned to UDID
    open "$SIMULATOR_FRONTEND"
  fi
  build
  xcrun simctl install "$SIMULATOR_UDID" "$APP_PATH"
  xcrun simctl launch --terminate-running-process "$SIMULATOR_UDID" "$APP_BUNDLE_ID"
}

case "${1:-}" in
  check) check ;;
  run) run ;;
  *) echo "usage: $0 {check|run}" >&2; exit 64 ;;
esac
