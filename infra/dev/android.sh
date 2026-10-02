#!/usr/bin/env bash
set -euo pipefail
umask 077

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
if [[ ${RUNNING_IN_CONTAINER:-${OPENPUSH_ANDROID_CONTAINER:-}} == 1 ]]; then
    ARTIFACTS=${OPENPUSH_ANDROID_ARTIFACTS:-/artifacts/android}
else
    ARTIFACTS=${OPENPUSH_ANDROID_ARTIFACTS:-"$ROOT/.opencode/dev/artifacts/android"}
fi
AVD_NAME=${OPENPUSH_ANDROID_AVD:-}
BOOT_TIMEOUT=${OPENPUSH_ANDROID_BOOT_TIMEOUT:-180}

usage() { echo "Usage: bash infra/dev/android.sh {build|emulator|deploy|smoke|sms} [args...]" >&2; }
die() { echo "android: $*" >&2; exit 1; }
is_wsl() { [[ -n ${WSL_INTEROP:-} ]] || grep -qi microsoft /proc/sys/kernel/osrelease /proc/version 2>/dev/null; }

windows_sdk_root() {
    # shellcheck disable=SC2016 # PowerShell variables deliberately reach powershell.exe verbatim.
    powershell.exe -NoProfile -Command '$root=[Environment]::GetEnvironmentVariable("ANDROID_SDK_ROOT", "User"); if (-not $root) {$root=Join-Path $env:LOCALAPPDATA "Android\\Sdk"}; $root' 2>/dev/null | tr -d '\r'
}
host_tool() {
    local tool=$1 root path
    if is_wsl; then
        root=${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$(windows_sdk_root)}}
        test -n "$root" || die "Windows Android SDK was not found; set ANDROID_SDK_ROOT to a Windows SDK path"
        [[ "$root" =~ ^[A-Za-z]:[\\/] ]] || die "Android SDK on WSL must be a Windows path for native .exe tools: $root"
        root=$(wslpath -u "$root")
        path="$root/$tool.exe"
    else
        root=${ANDROID_SDK_ROOT:-${ANDROID_HOME:-${HOME}/Library/Android/sdk}}
        test -d "$root" || die "Android SDK was not found; set ANDROID_SDK_ROOT"
        path="$root/$tool"
    fi
    test -x "$path" || die "Android SDK tool is missing or not executable: $path"
    printf '%s\n' "$path"
}
adb() { "$(host_tool platform-tools/adb)" "$@"; }
emulator_tool() { "$(host_tool emulator/emulator)" "$@"; }
windows_emulator_path() { wslpath -w "$(host_tool emulator/emulator)"; }
connected_emulators() { adb devices | tr -d '\r' | awk '$2 == "device" && $1 ~ /^emulator-/ { print $1 }'; }
known_emulators() { adb devices | tr -d '\r' | awk '$1 ~ /^emulator-/ { print $1 }'; }
adb_serial() {
    local serial=${OPENPUSH_ANDROID_SERIAL:-} item count=0 selected=
    if test -n "$serial"; then
        [[ "$serial" == emulator-* ]] || die "refusing physical device '$serial'; select an emulator serial"
        while IFS= read -r item; do [[ $item == "$serial" ]] && selected=$item; done < <(connected_emulators)
        test -n "$selected" || die "selected emulator is unavailable: $serial"
        printf '%s\n' "$selected"; return
    fi
    while IFS= read -r item; do selected=$item; count=$((count + 1)); done < <(connected_emulators)
    (( count == 1 )) || die "select exactly one running emulator with OPENPUSH_ANDROID_SERIAL (found $count)"
    printf '%s\n' "$selected"
}
apk() { printf '%s/%s\n' "$ARTIFACTS" "$1"; }
apk_for_adb() { if is_wsl; then wslpath -w "$(apk "$1")"; else apk "$1"; fi; }
ensure_debug_loopback() {
    local serial=$1 server=${OPENPUSH_DEBUG_SERVER:-http://127.0.0.1:8080} host port
    if [[ ! $server =~ ^http://(127\.0\.0\.1|localhost):([0-9]{1,5})$ ]]; then
        die "OPENPUSH_DEBUG_SERVER must be a loopback http URL with an explicit port"
    fi
    host=${BASH_REMATCH[1]}; port=${BASH_REMATCH[2]}
    (( 10#$port > 0 && 10#$port < 65536 )) || die "OPENPUSH_DEBUG_SERVER port is invalid"
    if is_wsl; then
        # The Windows adb host, not WSL curl, must be able to reach the debug server.
        local wslenv=${WSLENV:+$WSLENV:}
        # shellcheck disable=SC2016 # PowerShell reads its own environment variable literally.
        OPENPUSH_HEALTH_URL="http://$host:$port/healthz" WSLENV="${wslenv}OPENPUSH_HEALTH_URL/w" powershell.exe -NoProfile -Command '$response=Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 -Uri $env:OPENPUSH_HEALTH_URL; if ($response.StatusCode -lt 200 -or $response.StatusCode -ge 300) { exit 1 }' >/dev/null || die "Windows localhost server is not reachable from Windows: http://$host:$port/healthz"
    fi
    adb -s "$serial" reverse "tcp:$port" "tcp:$port"
}
container_build() {
    command -v docker >/dev/null || die "docker is required for android-build"
    docker compose version >/dev/null 2>&1 || die "Docker Compose v2 is required for android-build"
    test -f "$ROOT/.env" || die "missing .env; run bash infra/dev/dev.sh dev-setup first"
    mkdir -p "$ARTIFACTS"; chmod 0700 "$ARTIFACTS"
    local uid gid volume
    uid=$(id -u); gid=$(id -g)
    for volume in android-sdk android-gradle android-cargo android-target android-debug-keystore; do
        docker volume create "openpush-$volume-$uid-$gid" >/dev/null
    done
    DEV_UID=$uid DEV_GID=$gid docker compose --env-file "$ROOT/.env" -f "$ROOT/docker-compose.yml" -f "$ROOT/docker/compose.dev.yml" --profile android run --build --rm android run build
}
accept_licenses() {
    local status
    set +o pipefail
    yes | sdkmanager --licenses >/dev/null
    status=${PIPESTATUS[1]}
    set -o pipefail
    (( status == 0 )) || die "Android SDK license acceptance failed"
}
prepare_sdk() {
    : "${ANDROID_NDK_HOME:?Android NDK must be installed in the builder image}"
    : "${OPENPUSH_ACCEPT_ANDROID_LICENSES:?Set OPENPUSH_ACCEPT_ANDROID_LICENSES=1 after reviewing Android SDK licenses}"
    [[ $OPENPUSH_ACCEPT_ANDROID_LICENSES == 1 ]] || die "OPENPUSH_ACCEPT_ANDROID_LICENSES must equal 1"
    accept_licenses
    sdkmanager "platforms;android-35" "build-tools;35.0.0" "platform-tools" "ndk;27.2.12479018"
}
build() {
    if [[ ${RUNNING_IN_CONTAINER:-${OPENPUSH_ANDROID_CONTAINER:-}} == 1 ]]; then
        prepare_sdk
        bash infra/compose/verify-android-native.sh
        (cd apps/android && ./gradlew --no-daemon :app:assembleDebug :app:assembleDebugAndroidTest)
        mkdir -p "$ARTIFACTS"
        install -m 0644 apps/android/app/build/outputs/apk/debug/app-debug.apk "$ARTIFACTS/app-debug.apk"
        install -m 0644 apps/android/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk "$ARTIFACTS/app-debug-androidTest.apk"
    else
        container_build
    fi
}
serial_matches_avd() { [[ $(adb -s "$1" shell getprop ro.boot.qemu.avd_name 2>/dev/null | tr -d '\r') == "$AVD_NAME" ]]; }
running_avd_name() {
    local serial=$1 name
    name=$(adb -s "$serial" shell getprop ro.boot.qemu.avd_name 2>/dev/null | tr -d '\r')
    test -n "$name" || die "running emulator '$serial' did not report an AVD name; set OPENPUSH_ANDROID_AVD explicitly"
    printf '%s\n' "$name"
}
emulator() {
    if [[ ! $BOOT_TIMEOUT =~ ^[0-9]+$ ]] || (( 10#$BOOT_TIMEOUT <= 0 )); then
        die "OPENPUSH_ANDROID_BOOT_TIMEOUT must be a positive integer"
    fi
    local before known_before serial candidate matched owned_serial='' deadline pid='' started_at='' emulator_path='' helper_path='' existing=0 running_count=0 configured_count=0 configured_avd=''
    before=$(connected_emulators || true)
    if [[ -z "$AVD_NAME" ]]; then
        if test -n "${OPENPUSH_ANDROID_SERIAL:-}"; then
            serial=$(adb_serial)
            AVD_NAME=$(running_avd_name "$serial")
        else
            while IFS= read -r candidate; do
                test -n "$candidate" || continue
                serial=$candidate
                running_count=$((running_count + 1))
            done <<<"$before"
            if (( running_count > 1 )); then
                die "multiple running emulators found; set OPENPUSH_ANDROID_SERIAL to one emulator serial"
            elif (( running_count == 1 )); then
                AVD_NAME=$(running_avd_name "$serial")
            else
                while IFS= read -r candidate; do
                    [[ "$candidate" =~ ^[A-Za-z0-9._-]+$ ]] || continue
                    configured_avd=$candidate
                    configured_count=$((configured_count + 1))
                done < <(emulator_tool -list-avds | tr -d '\r')
                (( configured_count == 1 )) || die "no running emulator and $configured_count configured AVDs found; set OPENPUSH_ANDROID_AVD to an existing AVD or create one explicitly"
                AVD_NAME=$configured_avd
            fi
        fi
    fi
    before=$(connected_emulators || true)
    known_before=$(known_emulators || true)
    while IFS= read -r serial; do
        test -n "$serial" || continue
        if serial_matches_avd "$serial"; then
            existing=$((existing + 1))
            matched=$serial
        fi
    done <<<"$before"
    (( existing <= 1 )) || die "multiple running emulators match AVD '$AVD_NAME'; select one with OPENPUSH_ANDROID_SERIAL"
    if (( existing == 1 )); then
        if test -n "${OPENPUSH_ANDROID_SERIAL:-}" && [[ $matched != "$OPENPUSH_ANDROID_SERIAL" ]]; then
            die "selected serial does not run AVD '$AVD_NAME'"
        fi
        serial=$matched
        deadline=$((SECONDS + 10#$BOOT_TIMEOUT))
    else
        emulator_tool -list-avds | tr -d '\r' | grep -Fx -- "$AVD_NAME" >/dev/null || die "AVD '$AVD_NAME' does not exist; create it explicitly"
        if is_wsl; then
            helper_path=$(wslpath -w "$ROOT/infra/dev/windows-emulator.ps1")
            emulator_path=$(windows_emulator_path)
            local launch_record
            launch_record=$(powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$helper_path" -Action start -EmulatorPath "$emulator_path" -Avd "$AVD_NAME" | tr -d '\r') || die "Windows emulator launch failed"
            pid=${launch_record%%|*}; started_at=${launch_record#*|}
            [[ $pid =~ ^[0-9]+$ && $started_at =~ ^[0-9]+$ ]] || die "Windows emulator helper returned an invalid process identity"
        else
            emulator_path=$(host_tool emulator/emulator)
            mkdir -p "$ARTIFACTS"; nohup "$emulator_path" -avd "$AVD_NAME" >"$ARTIFACTS/emulator.log" 2>&1 & pid=$!
        fi
        deadline=$((SECONDS + 10#$BOOT_TIMEOUT))
        serial=
    fi
    cleanup_emulator() {
        test -n "$pid" || return 0
        # Windows cleanup uses verified native process identity, not an inferred adb serial.
        if ! is_wsl && test -n "$owned_serial" && kill -0 "$pid" 2>/dev/null; then adb -s "$owned_serial" emu kill >/dev/null 2>&1 || true; fi
        if is_wsl; then powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$helper_path" -Action stop -EmulatorPath "$emulator_path" -Avd "$AVD_NAME" -ProcessId "$pid" -StartedAt "$started_at" >/dev/null 2>&1 || true
        else kill "$pid" 2>/dev/null || true; fi
    }
    trap cleanup_emulator EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    trap 'exit 129' HUP
    if (( existing == 0 )); then
        while (( SECONDS < deadline )); do
            while IFS= read -r candidate; do
                if ! is_wsl && ! kill -0 "$pid" 2>/dev/null; then
                    die "emulator process exited; see $ARTIFACTS/emulator.log"
                fi
                if ! grep -Fx -- "$candidate" <<<"$known_before" >/dev/null && serial_matches_avd "$candidate"; then
                    if ! is_wsl && ! kill -0 "$pid" 2>/dev/null; then
                        die "emulator process exited; see $ARTIFACTS/emulator.log"
                    fi
                    owned_serial=$candidate
                    serial=$candidate
                    break 2
                fi
            done < <(connected_emulators)
            sleep 2
        done
        test -n "$serial" || die "emulator did not appear within ${BOOT_TIMEOUT}s"
    fi
    until [[ $(adb -s "$serial" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r') == 1 ]]; do
        (( SECONDS < deadline )) || die "emulator did not boot within ${BOOT_TIMEOUT}s"
        sleep 2
    done
    trap - EXIT INT TERM HUP
    echo "Android emulator ready: $serial"
}
ensure_supported_abi() {
    local abi
    abi=$(adb -s "$1" shell getprop ro.product.cpu.abi 2>/dev/null | tr -d '\r')
    [[ $abi == arm64-v8a || $abi == x86_64 ]] || die "emulator ABI '$abi' is unsupported; select an arm64-v8a or x86_64 AVD"
}
deploy() { local serial; serial=$(adb_serial); test -f "$(apk app-debug.apk)" || die "APK missing; run android-build first"; ensure_supported_abi "$serial"; ensure_debug_loopback "$serial"; adb -s "$serial" install -r "$(apk_for_adb app-debug.apk)"; }
smoke() {
    local serial output status
    serial=$(adb_serial)
    if ! test -f "$(apk app-debug.apk)" || ! test -f "$(apk app-debug-androidTest.apk)"; then die "APK(s) missing; run android-build first"; fi
    ensure_supported_abi "$serial"
    ensure_debug_loopback "$serial"
    adb -s "$serial" install -r "$(apk_for_adb app-debug.apk)"
    adb -s "$serial" install -r "$(apk_for_adb app-debug-androidTest.apk)"
    local output_file
    output_file=$(mktemp)
    set +e; adb -s "$serial" shell am instrument -r -w dev.openpush.mobile.test/androidx.test.runner.AndroidJUnitRunner >"$output_file" 2>&1; status=$?; set -e
    output=$(tr -d '\r' <"$output_file"); rm -f "$output_file"
    printf '%s\n' "$output"
    if (( status != 0 )) || grep -Eq 'FAILURES!!!|INSTRUMENTATION_FAILED|Process crashed|INSTRUMENTATION_CODE: (0|-[2-9][0-9]*|[1-9][0-9]*)' <<<"$output" || ! grep -Eq '^OK \([1-9][0-9]* tests?\)' <<<"$output" || ! grep -Eq '^INSTRUMENTATION_CODE: -1$' <<<"$output"; then die "instrumentation smoke failed"; fi
}
sms() { local serial; serial=$(adb_serial); [[ $# == 2 && -n $1 && -n $2 ]] || die "Usage: android.sh sms <number> <message>"; echo "Synthetic emulator SMS simulation only; no carrier message is sent." >&2; adb -s "$serial" emu sms send "$1" "$2"; }

case ${1:-} in
    build) build ;; test) [[ ${RUNNING_IN_CONTAINER:-${OPENPUSH_ANDROID_CONTAINER:-}} == 1 ]] || die "test is only available inside the Android runner"; prepare_sdk; cargo build --locked -p openpush-mobile-bindings; (cd apps/android && ./gradlew --no-daemon :app:testDebugUnitTest :app:lintDebug) ;;
    emulator) emulator ;; deploy) deploy ;; smoke) smoke ;; sms) shift; sms "$@" ;; *) usage; exit 64 ;;
esac
