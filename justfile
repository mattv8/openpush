set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments
set dotenv-path := ".opencode/dev/android.env"
set dotenv-required := false

compose := "docker compose --env-file .env -f docker-compose.yml"
dev_compose := "DEV_UID=$(id -u) DEV_GID=$(id -g) docker compose --env-file .env -f docker-compose.yml -f docker/compose.dev.yml"
dev_prepare := "mkdir -p .opencode/dev/artifacts && chmod 700 .opencode/dev/artifacts && for cache in cargo pnpm target; do docker volume create openpush-dev-${cache}-$(id -u)-$(id -g) >/dev/null; done"
dev_prereq := "command -v docker >/dev/null || { echo 'docker is required for container development' >&2; exit 1; }; docker compose version >/dev/null || { echo 'docker compose is required for container development' >&2; exit 1; }; test -f .env || { echo '.env is required; run bash infra/dev/dev.sh dev-setup' >&2; exit 1; }"

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
    {{ dev_prereq }}
    {{ dev_prepare }}
    {{ dev_compose }} up --build --detach --wait

dev-down:
    {{ dev_prereq }}
    {{ dev_compose }} down

dev-setup:
    bash infra/dev/dev.sh dev-setup

dev-actions:
    bash infra/dev/dev.sh dev-actions

dev-start:
    bash infra/dev/dev.sh dev-setup
    just dev-up
    bash infra/dev/android.sh emulator
    just desktop-run

dev-build:
    {{ dev_prereq }}
    {{ dev_prepare }}
    {{ dev_compose }} exec dev run build server

dev-test:
    {{ dev_prereq }}
    {{ dev_prepare }}
    {{ dev_compose }} exec dev run test rust

dev-demo:
    bash infra/dev/dev.sh dev-demo

android-build *args:
    bash infra/dev/android.sh build "$@"

android-emulator:
    bash infra/dev/android.sh emulator

android-deploy:
    bash infra/dev/android.sh deploy

android-open:
    bash infra/dev/android.sh open

android-run:
    @test "${OPENPUSH_ACCEPT_ANDROID_LICENSES:-}" = 1 || { echo "OPENPUSH_ACCEPT_ANDROID_LICENSES=1 is required before Android build/deploy; review and accept Android SDK licenses first, then make the one-time setting in .opencode/dev/android.env" >&2; exit 1; }
    just dev-up
    bash infra/dev/android.sh build
    bash infra/dev/android.sh emulator
    bash infra/dev/android.sh deploy
    bash infra/dev/android.sh open

android-smoke:
    bash infra/dev/android.sh smoke

android-sms *args:
    bash infra/dev/android.sh sms "$@"

desktop-dev:
    bash infra/dev/desktop.sh dev

desktop-bundle:
    bash infra/dev/desktop.sh build

desktop-run:
    bash infra/dev/desktop.sh build && bash infra/dev/desktop.sh open

desktop-open:
    bash infra/dev/desktop.sh open

smoke-infra:
    @test -f .env || { echo ".env is required; copy .env.example and set synthetic development credentials" >&2; exit 1; }
    {{ compose }} up --detach --wait
    @curl --fail --silent --show-error http://127.0.0.1:7000/healthz
    @curl --fail --silent --show-error http://127.0.0.1:7000/readyz
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
    cargo deny check advisories
    python3 infra/audit/check-vendored.py

audit-secrets:
    @scan=$(mktemp -d); trap 'rm -rf "$scan"' EXIT; git ls-files -co --exclude-standard -z | tar --null -T - -cf - | tar -xf - -C "$scan"; gitleaks detect --source "$scan" --no-git --redact --exit-code 1 --config .gitleaks.toml

backup destination:
    ./infra/compose/backup.sh {{ quote(destination) }}

restore archive project:
    ./infra/compose/restore.sh {{ quote(archive) }} {{ quote(project) }}
