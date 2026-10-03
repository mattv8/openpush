#!/usr/bin/env bash
# Build development-only iOS artifacts. The device archive is deliberately unsigned.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

DEVELOPER_DIR="${DEVELOPER_DIR:-$(xcode-select -p)}"
export DEVELOPER_DIR
xcodebuild_out=$(xcodebuild -version)
xcodebuild_major=$(printf '%s' "$xcodebuild_out" | head -1 | awk '{print $2}' | cut -d. -f1)
test "$xcodebuild_major" -ge 26 || { echo "Xcode major version $xcodebuild_major is less than 26." >&2; exit 1; }
xcrun -sdk iphoneos -find clang > /dev/null || { echo 'iphoneos SDK not available.' >&2; exit 1; }
xcrun -sdk iphonesimulator -find clang > /dev/null || { echo 'iphonesimulator SDK not available.' >&2; exit 1; }
echo "$xcodebuild_out"

cargo build -p peppy-mobile-bindings --locked
generated_tmp=$(mktemp -d "${TMPDIR:-/tmp}/peppy-ios-generated.XXXXXX")
trap 'rm -rf "$generated_tmp"' EXIT
cargo run --locked -p peppy-mobile-bindings --features cli --bin uniffi-bindgen -- \
  generate --library target/debug/libpeppy_mobile_bindings.dylib --language swift --out-dir "$generated_tmp"
if ! diff -ru --exclude=module.modulemap "apps/ios/Generated" "$generated_tmp"; then
  echo 'Generated iOS bindings drift from the Rust source; regenerate apps/ios/Generated before building artifacts.' >&2
  exit 1
fi

build_rust() {
  local target=$1 sdk=$2
  local target_env target_upper sdkroot clang ar ranlib deployment_flag target_triple compiler_wrapper
  target_env=${target//-/_}
  target_upper=$(printf '%s' "$target_env" | tr '[:lower:]' '[:upper:]')
  sdkroot=$(xcrun --sdk "$sdk" --show-sdk-path)
  clang=$(xcrun --sdk "$sdk" --find clang)
  ar=$(xcrun --sdk "$sdk" --find ar)
  ranlib=$(xcrun --sdk "$sdk" --find ranlib)
  case "$sdk" in
    iphoneos)
      target_triple=arm64-apple-ios26.0
      deployment_flag=-miphoneos-version-min=26.0
      ;;
    iphonesimulator)
      target_triple=arm64-apple-ios26.0-simulator
      deployment_flag=-mios-simulator-version-min=26.0
      ;;
    *) echo "unsupported iOS SDK: $sdk" >&2; exit 1 ;;
  esac
  compiler_wrapper=$(mktemp "${TMPDIR:-/tmp}/peppy-ios-clang.XXXXXX")
  cat > "$compiler_wrapper" <<'EOF'
#!/bin/sh
exec "$PEPPY_IOS_CLANG" "$@" -target "$PEPPY_IOS_TARGET" -isysroot "$PEPPY_IOS_SDKROOT" "$PEPPY_IOS_DEPLOYMENT_FLAG"
EOF
  chmod +x "$compiler_wrapper"
  env \
    SDKROOT="$sdkroot" \
    IPHONEOS_DEPLOYMENT_TARGET=26.0 \
    PEPPY_IOS_CLANG="$clang" \
    PEPPY_IOS_SDKROOT="$sdkroot" \
    PEPPY_IOS_TARGET="$target_triple" \
    PEPPY_IOS_DEPLOYMENT_FLAG="$deployment_flag" \
    CFLAGS="-target $target_triple -isysroot $sdkroot $deployment_flag" \
    "CC_$target_env=$compiler_wrapper" \
    "AR_$target_env=$ar" \
    "RANLIB_$target_env=$ranlib" \
    "CARGO_TARGET_${target_upper}_LINKER=$compiler_wrapper" \
    AR="$ar" \
    RANLIB="$ranlib" \
    cargo build -p peppy-mobile-bindings --locked --release --target "$target"
  rm -f "$compiler_wrapper"
}

rustup target add aarch64-apple-ios aarch64-apple-ios-sim
build_rust aarch64-apple-ios iphoneos
build_rust aarch64-apple-ios-sim iphonesimulator

dist="$root/dist/ios"
rm -rf "$dist"
mkdir -p "$dist"

xcodebuild -project apps/ios/PeppyMobile.xcodeproj -scheme PeppyMobile \
  -configuration Release -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath "$root/.build/ios-simulator" ARCHS=arm64 ONLY_ACTIVE_ARCH=YES CODE_SIGNING_ALLOWED=NO build
tar -C "$root/.build/ios-simulator/Build/Products/Release-iphonesimulator" \
  -czf "$dist/PeppyMobile-simulator.app.tar.gz" PeppyMobile.app

xcodebuild -project apps/ios/PeppyMobile.xcodeproj -scheme PeppyMobile \
  -configuration Release -sdk iphoneos -destination 'generic/platform=iOS' \
  -archivePath "$root/.build/PeppyMobile-unsigned-device.xcarchive" \
  ARCHS=arm64 CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY='' archive
tar -C "$root/.build" -czf "$dist/PeppyMobile-unsigned-device.xcarchive.tar.gz" \
  PeppyMobile-unsigned-device.xcarchive

cat > "$dist/README.txt" <<'EOF'
Peppy iOS development artifacts

PeppyMobile-simulator.app.tar.gz is an ARM64 iOS Simulator application bundle.
PeppyMobile-unsigned-device.xcarchive.tar.gz is an unsigned device archive.
It is not an installable IPA and cannot be installed on a device without signing.
EOF
