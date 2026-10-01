#!/usr/bin/env bash
set -euo pipefail

: "${ANDROID_NDK_HOME:?ANDROID_NDK_HOME must name the installed Android NDK}"
ndk_prebuilt_root="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt"
ndk_host=$(find "$ndk_prebuilt_root" -mindepth 1 -maxdepth 1 -type d -print -quit)
test -n "$ndk_host" || { echo "Android NDK LLVM toolchain is missing" >&2; exit 1; }
ndk_bin="$ndk_host/bin"
clang="$ndk_bin/aarch64-linux-android26-clang"
llvm_ar="$ndk_bin/llvm-ar"
llvm_ranlib="$ndk_bin/llvm-ranlib"
llvm_readelf="$ndk_bin/llvm-readelf"
for tool in "$clang" "$llvm_ar" "$llvm_ranlib" "$llvm_readelf"; do
    test -x "$tool" || { echo "Android NDK tool is missing: $tool" >&2; exit 1; }
done

# Host generation deliberately runs before Android's global AR/RANLIB are scoped in the
# subshell below. Never feed NDK archive tools to the host build.
cargo build --locked -p openpush-mobile-bindings
host_library=$(find target/debug -maxdepth 1 -type f \( -name 'libopenpush_mobile_bindings.so' -o -name 'libopenpush_mobile_bindings.dylib' \) -print -quit)
test -n "$host_library" || { echo "Host mobile-bindings library is missing" >&2; exit 1; }
cargo run --locked -p openpush-mobile-bindings --features cli --bin uniffi-bindgen -- \
    generate --library "$host_library" --language kotlin --out-dir apps/android/app/src/main/java

(
    export PATH="$ndk_bin:$PATH"
    export CC="$clang"
    export AR="$llvm_ar"
    export RANLIB="$llvm_ranlib"
    export CC_aarch64_linux_android="$clang"
    export AR_aarch64_linux_android="$llvm_ar"
    export RANLIB_aarch64_linux_android="$llvm_ranlib"
    export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$clang"
    export CARGO_TARGET_AARCH64_LINUX_ANDROID_AR="$llvm_ar"
    export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-Wl,--no-undefined"

    # An earlier host-ar cross-build can leave a valid-looking, empty libsodium.a in Cargo's
    # package cache. Invalidate only this package for the Android target before rebuilding.
    cargo clean --target aarch64-linux-android -p libsodium-sys-stable

    # Hyphenated target-qualified variables cannot be shell identifiers, so provide them
    # through env while retaining the global and underscore forms required by configure/cc.
    env \
        "CC_aarch64-linux-android=$clang" \
        "AR_aarch64-linux-android=$llvm_ar" \
        "RANLIB_aarch64-linux-android=$llvm_ranlib" \
        cargo build --locked -p openpush-mobile-bindings --lib --target aarch64-linux-android
)

android_library=target/aarch64-linux-android/debug/libopenpush_mobile_bindings.so
test -s "$android_library" || { echo "Android mobile-bindings library is missing" >&2; exit 1; }

unresolved=$(
    "$llvm_readelf" --dyn-syms --wide "$android_library" |
        awk '$7 == "UND" { sub(/@.*/, "", $8); print $8 }' |
        grep -E '^(sodium_|randombytes_|crypto_)' || true
)
if test -n "$unresolved"; then
    echo "Android native library retains unresolved crypto symbols:" >&2
    echo "$unresolved" >&2
    exit 1
fi

mkdir -p apps/android/app/src/main/jniLibs/arm64-v8a
cp "$android_library" apps/android/app/src/main/jniLibs/arm64-v8a/
