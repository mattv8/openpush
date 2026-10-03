# Peppy Android gateway

Follow the canonical [contributing workflow](../../CONTRIBUTING.md) for setup, container builds, emulator operations, and checks.

This native Kotlin/Compose app is a **gateway phone**, not a replacement for the default messaging app. Once enrolled and unlocked, its bottom navigation has **Mirroring**, **SMS**, and **Account** tabs; Settings is available from the enrolled shell. Subscriber conversations and composition remain in the shared webview product surface, not in this app.

## Enrollment and account

Pair from the desktop's owner-approved QR flow or import a v1 credential JSON file. The QR payload is JSON with exactly `https_origin` and `intent_token`; it contains no vault metadata, passphrase, owner token, or private key. The phone creates and keeps its signing key in native secure storage, verifies the matching locally computed SAS, then waits for owner approval before consuming the challenge. The existing shared vault passphrase is entered manually to unlock keys and is not stored.

The Account tab shows the server-reported role and device roster. A device may disconnect itself; only a server-reported `owner` can remove another device or reveal the typed-`ERASE` vault-delete control. A gateway role does not grant owner privileges. Disconnect archives local encrypted state; it is not a reset or a claim that a revoked credential can resume sync.

## Gateway policy and platform limits

Durable gateway settings and policy decisions live in the shared Rust core and are exposed through generated bindings. Android supplies transient facts such as granted permissions, notification-listener access, Wi-Fi transport, and silent-notification status; it does not maintain a second policy store. New mirroring is off by default. SMS and MMS synchronization, skip-silent, and Wi-Fi-only media settings are capability- and permission-gated. A Wi-Fi-only setting means Android's actual Wi-Fi transport, not merely an unmetered connection.

Android can capture supported SMS broadcasts and submit permitted SMS commands through the public carrier API. It requests `RECEIVE_SMS` and `SEND_SMS`; experimental MMS capture additionally requires `READ_SMS` and `RECEIVE_MMS`. Notification mirroring requires the user to enable Android's notification-listener access. Peppy does not request `WRITE_SMS`, the default-SMS role, or hidden carrier APIs. SIM availability or default-SMS status does not provide general RCS access; RCS remains unavailable.

Commands use the current default SMS subscription. If it changes or disappears, an old-route command remains pending rather than moving to another SIM. MMS does not downgrade to SMS. Carrier behavior, restricted-permission approval, and physical SMS/MMS delivery require device and carrier verification; emulator and unit results do not establish them.

## Relay wakes and encryption boundary

The optional FCM wake relay is **off by default**. An operator must configure the relay and the user must explicitly enable it; without Firebase configuration, registration waits and normal bounded sync still runs while the app is open. FCM receives only content-free `wake` or `challenge` values and a wake only schedules work—it never authorizes carrier work. This repository does not claim a live signed FCM delivery path.

Vault sync envelopes and private attachment copies retain the existing encrypted body transport. Carrier SMS/MMS itself is outside that encryption boundary. The server's private attachment store is encrypted; public derivatives require a separate explicit publication action.

## Contacts and experimental MMS

Contact sync is separately enabled and permission-gated. The phone publishes its own book and applies remote edits according to its local Auto / Confirm / Off policy; permission loss and incomplete scans do not imply deletion. Photos are private encrypted 256×256 JPEG attachments, capped at 64 KiB. Doze, force-stop, provider behavior, ContactsProvider behavior, and background work can delay synchronization.

Experimental MMS requires participating clients that advertise MMS content version 2 or later. The existing messaging app downloads carrier MMS; Peppy scans available provider parts and uploads completed encrypted records. The phone validates encoded size against the reported carrier limit or a conservative 300 KiB fallback. Confirmed send is distinct from delivery, uncertain attempts are not automatically resent, and carrier MMS remains outside Peppy's encryption boundary.

For group MMS replies, the app reads the default SMS SIM's own number from the carrier (via `READ_PHONE_NUMBERS` on Android 13+, or the existing `READ_SMS` on older releases) solely so replies to a group MMS go to everyone except this phone. If the carrier does not provide the number, the MMS settings offer manual entry.

## Generate and verify

Generated Kotlin is Rust-owned output under `app/src/main/java`; do not edit it manually. From the repository root:

```sh
cargo build --locked -p peppy-mobile-bindings
cargo run --locked -p peppy-mobile-bindings --features cli --bin uniffi-bindgen -- generate --library target/debug/libpeppy_mobile_bindings.dylib --language kotlin --out-dir apps/android/app/src/main/java

export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/27.2.12479018"
infra/compose/verify-android-native.sh
cd apps/android
./gradlew :jvm-smoke:run :app:testDebugUnitTest :app:lintDebug :app:assembleDebug :app:assembleDebugAndroidTest
```

The Android build needs JDK 17, command-line tools, `platform-tools`, `platforms;android-36`, `build-tools;35.0.0`, and `ndk;27.2.12479018`; set `JAVA_HOME` and `ANDROID_HOME` or `ANDROID_SDK_ROOT`. The native verifier checks `arm64-v8a` and `x86_64` libraries and crypto symbols. `just android-smoke` additionally requires a selected emulator and verifies instrumentation execution, but neither it nor the host checks prove carrier, FCM, store, or physical-network behavior.
