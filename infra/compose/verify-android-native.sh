#!/usr/bin/env bash
set -euo pipefail

: "${ANDROID_NDK_HOME:?ANDROID_NDK_HOME must name the installed Android NDK}"
target_dir=${CARGO_TARGET_DIR:-target}
ndk_prebuilt_root="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt"
ndk_host=$(find "$ndk_prebuilt_root" -mindepth 1 -maxdepth 1 -type d -print -quit)
test -n "$ndk_host" || { echo "Android NDK LLVM toolchain is missing" >&2; exit 1; }
ndk_bin="$ndk_host/bin"
llvm_ar="$ndk_bin/llvm-ar"
llvm_ranlib="$ndk_bin/llvm-ranlib"
llvm_readelf="$ndk_bin/llvm-readelf"
for tool in "$llvm_ar" "$llvm_ranlib" "$llvm_readelf"; do
    test -x "$tool" || { echo "Android NDK tool is missing: $tool" >&2; exit 1; }
done

# Generate Kotlin from the host library before target-scoped NDK tools are exported.
cargo build --locked -p openpush-mobile-bindings
host_library=$(find "$target_dir/debug" -maxdepth 1 -type f \( -name 'libopenpush_mobile_bindings.so' -o -name 'libopenpush_mobile_bindings.dylib' \) -print -quit)
test -n "$host_library" || { echo "Host mobile-bindings library is missing" >&2; exit 1; }
cargo run --locked -p openpush-mobile-bindings --features cli --bin uniffi-bindgen -- generate --library "$host_library" --language kotlin --out-dir apps/android/app/src/main/java

build_target() {
    local target=$1 abi=$2 clang="$ndk_bin/$3"
    local cargo_target upper_target
    cargo_target=$(printf '%s' "$target" | tr '-' '_')
    upper_target=$(printf '%s' "$cargo_target" | tr '[:lower:]' '[:upper:]')
    test -x "$clang" || { echo "Android NDK compiler is missing: $clang" >&2; exit 1; }
    (
        export PATH="$ndk_bin:$PATH" CC="$clang" AR="$llvm_ar" RANLIB="$llvm_ranlib"
        export "CC_$cargo_target=$clang" "AR_$cargo_target=$llvm_ar" "RANLIB_$cargo_target=$llvm_ranlib"
        export "CARGO_TARGET_${upper_target}_LINKER=$clang" "CARGO_TARGET_${upper_target}_AR=$llvm_ar"
        export "CARGO_TARGET_${upper_target}_RUSTFLAGS=-C link-arg=-Wl,--no-undefined"
        cargo clean --target "$target" -p libsodium-sys-stable
        env "CC_$target=$clang" "AR_$target=$llvm_ar" "RANLIB_$target=$llvm_ranlib" \
            cargo build --locked -p openpush-mobile-bindings --lib --target "$target"
    )
    local library="$target_dir/$target/debug/libopenpush_mobile_bindings.so"
    test -s "$library" || { echo "Android mobile-bindings library is missing: $library" >&2; exit 1; }
    local unresolved
    unresolved=$("$llvm_readelf" --dyn-syms --wide "$library" | awk '$7 == "UND" { sub(/@.*/, "", $8); print $8 }' | grep -E '^(sodium_|randombytes_|crypto_)' || true)
    test -z "$unresolved" || { echo "Android $target library retains unresolved crypto symbols:" >&2; echo "$unresolved" >&2; exit 1; }
    mkdir -p "apps/android/app/src/main/jniLibs/$abi"
    cp "$library" "apps/android/app/src/main/jniLibs/$abi/"
}

build_target aarch64-linux-android arm64-v8a aarch64-linux-android26-clang
build_target x86_64-linux-android x86_64 x86_64-linux-android26-clang
