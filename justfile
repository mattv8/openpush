set shell := ["bash", "-euo", "pipefail", "-c"]

compose := "docker compose --env-file .env -f docker-compose.yml"

default:
    @just --list

version *args:
    infra/release/compute-version.sh {{ args }}

release-test:
    infra/release/test-compute-version.sh
    infra/release/test-release-scripts.sh

doctor:
    @command -v cargo >/dev/null || { echo "cargo is required; install the pinned Rust toolchain" >&2; exit 1; }
    @command -v rustc >/dev/null || { echo "rustc is required; install the pinned Rust toolchain" >&2; exit 1; }
    @command -v node >/dev/null || { echo "node is required" >&2; exit 1; }
    @command -v pnpm >/dev/null || { echo "pnpm is required" >&2; exit 1; }
    @command -v docker >/dev/null || { echo "docker is required" >&2; exit 1; }
    @expected_rust=$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml); actual_rust=$(rustc -V | awk '{print $2}'); test "$actual_rust" = "$expected_rust" || { echo "rustc $expected_rust is required (found $actual_rust)" >&2; exit 1; }
    @expected_node=$(cat .node-version); actual_node=$(node -p 'process.versions.node'); test "$actual_node" = "$expected_node" || { echo "Node $expected_node is required (found $actual_node)" >&2; exit 1; }
    @test "$(pnpm --version)" = "12.8.1" || { echo "pnpm 12.8.1 is required" >&2; exit 1; }
    @cargo --version && pnpm --version && docker version && docker compose version
    @test -f .env || { echo ".env is required; copy .env.example and set synthetic development credentials" >&2; exit 1; }
    @{{ compose }} config --quiet
    @android_sdk="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"; if test -n "$android_sdk" && test -d "$android_sdk"; then echo "Android SDK: $android_sdk"; elif test -d "$HOME/Library/Android/sdk"; then echo "Android SDK: installed at $HOME/Library/Android/sdk but ANDROID_HOME/ANDROID_SDK_ROOT is not configured"; else echo "Android SDK: missing (required for just android-test)"; fi
    @if xcode-select -p >/dev/null 2>&1 && xcrun --sdk iphonesimulator --show-sdk-path >/dev/null 2>&1; then echo "iOS SDK: available"; else echo "iOS SDK: unavailable (full Xcode is required for an iOS simulator build)"; fi

dev-up:
    @test -f .env || { echo ".env is required; copy .env.example and set synthetic development credentials" >&2; exit 1; }
    {{ compose }} up --detach --wait

dev-down:
    {{ compose }} down

smoke-infra:
    @test -f .env || { echo ".env is required; copy .env.example and set synthetic development credentials" >&2; exit 1; }
    {{ compose }} up --detach --wait
    @curl --fail --silent --show-error http://127.0.0.1:8080/healthz
    @curl --fail --silent --show-error http://127.0.0.1:8080/readyz
    {{ compose }} run --rm api storage-check

cargo-fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace --locked

integration-test:
    source infra/compose/test-env.sh; trap openpush_test_infra_down EXIT; openpush_test_infra_up; cargo test --workspace --locked

server-test:
    cargo test -p openpush-server --locked

contracts-check:
    cargo run --locked -p openpush-protocol --bin generate-contracts -- --check

desktop-build:
    pnpm --filter @openpush/desktop build

desktop-test:
    pnpm --filter @openpush/desktop-ui test
    pnpm --filter @openpush/desktop test

ffi-smoke:
    cargo build -p openpush-mobile-bindings --locked
    cargo run --locked -p openpush-mobile-bindings --features cli --bin uniffi-bindgen -- generate --library target/debug/libopenpush_mobile_bindings.dylib --language kotlin --out-dir apps/android/app/src/main/java
    cargo run --locked -p openpush-mobile-bindings --features cli --bin uniffi-bindgen -- generate --library target/debug/libopenpush_mobile_bindings.dylib --language swift --out-dir apps/ios/Generated
    cargo test -p openpush-mobile-bindings --locked
    cd apps/ios && DYLD_LIBRARY_PATH="$PWD/../../target/debug" swift run OpenPushMobileSmoke

android-test:
    cd apps/android && ./gradlew :jvm-smoke:run :app:testDebugUnitTest :app:lintDebug :app:assembleDebug

ios-test:
    cd apps/ios && DYLD_LIBRARY_PATH="$PWD/../../target/debug" swift test --no-parallel
    cd apps/ios && DYLD_LIBRARY_PATH="$PWD/../../target/debug" swift run OpenPushMobileSmoke

storage-contract:
    @test -f .env || { echo ".env is required" >&2; exit 1; }
    {{ compose }} up --detach --wait
    {{ compose }} run --rm api storage-check

audit-dependencies:
    cargo deny check licenses bans sources

audit-secrets:
    @scan=$(mktemp -d); trap 'rm -rf "$scan"' EXIT; git ls-files -co --exclude-standard -z | tar --null -T - -cf - | tar -xf - -C "$scan"; gitleaks detect --source "$scan" --no-git --redact --exit-code 1 --config .gitleaks.toml

backup destination:
    ./infra/compose/backup.sh {{ quote(destination) }}

restore archive project:
    ./infra/compose/restore.sh {{ quote(archive) }} {{ quote(project) }}
