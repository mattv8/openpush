# Peppy Android companion shell

Follow the canonical [contributing workflow](../../CONTRIBUTING.md) for setup, container builds, emulator operations, and checks.

This Kotlin/Compose companion captures new SMS broadcasts and submits permitted SMS commands through Android's carrier API. Experimental MMS adds provider capture, optional history import, encrypted attachment synchronization, and a public `SmsManager` send adapter. It remains a companion to the default messaging app. SMS uses `RECEIVE_SMS` and `SEND_SMS`; enabling MMS capture additionally requires `READ_SMS` and `RECEIVE_MMS`. Peppy never requests `WRITE_SMS` or the default-SMS role.

Sending uses the current default SMS subscription. If that subscription changes or disappears, a command for the old route remains pending and does not move to another SIM. MMS never downgrades to SMS. Ordinary Android apps have no general RCS inbox/send API; default-SMS status alone does not grant RCS access. Peppy uses no hidden carrier APIs. Emulator and unit results do not prove carrier or store behavior.

## Contact sync

Enable **Sync contacts** and grant Contacts access. The phone publishes its own
address book; the desktop sends requests for the phone to apply. Read-only access
supports capture but refuses remote writes. Permission loss and incomplete scans
never imply deletion.

The phone controls **Save new contacts to** and the **Auto / Confirm / Off** remote
edit policy. Auto is the default; large deletions still require phone approval.
Updates preserve the original writable account's fields. Read-only fields cannot
be edited, and an uncertain interrupted write is reconciled instead of repeated.
**Stop and retire** leaves OS contacts untouched and marks the published book retired.

Captured fields include structured names, nickname, labeled phones and emails,
organization/title, postal addresses, birthday and notes (up to 8 KiB). Contact
photos become private encrypted 256×256 JPEG attachments of at most 64 KiB.
Full-resolution originals are not replicated or published as public image copies.

The existing network-constrained WorkManager pass performs capture, encrypted
media transfer and permitted edits. A debounced contact observer requests work
while the process is running; the 15-minute periodic worker provides recovery.
Doze and force-stop can delay work. Incremental provider timestamps reduce reads;
bounded full scans reconcile missing records. Progress and write evidence live in
the encrypted Rust core. Real ContactsProvider, DisplayPhoto and WorkManager
behavior require device verification beyond fake-provider and host tests.

## Experimental MMS

Upgrade participating clients before enabling MMS on the phone. The desktop requires the selected gateway to advertise MMS content version 2 or later. Enable the experimental setting, grant the requested permissions, and confirm your own number for the selected SIM before replying to incoming groups. The number stays in the encrypted vault, not the public capability report.

The existing messaging app downloads incoming carrier MMS. Peppy reads available provider parts and keeps pending acquisition work locally. Incomplete or unavailable messages appear in the phone's MMS health view; they do not appear on the desktop until capture and encrypted uploads finish. History import is opt-in and imported messages start read. Force-stop, Doze, provider write timing, and default-app download settings can delay capture.

Messages support text-only groups and attachments without text. Storage limits are not carrier limits: the phone validates the complete encoded MMS against the reported carrier size limit, or a conservative 300 KiB application fallback. Oversize messages are not silently compressed or replaced with public links. Confirmed send results are distinct from delivery; an uncertain attempt is never automatically resent. Unknown-attempt PDU files remain for at least seven days and are cleaned during subsequent gateway work; the durable attempt record remains.

The server stores encrypted attachment copies in its existing private S3-compatible store. Public image copies require a separate explicit publication action. Carrier SMS/MMS itself is outside Peppy's encryption boundary.

Physical MMS interoperability, carrier-specific behavior and Google Play's restricted-permission approval remain separate acceptance gates. The experimental switch does not certify a phone or carrier. Full RCS remains blocked pending a legitimate privileged carrier/OEM integration; Google's business-messaging API is not a personal-inbox substitute.

Generated Kotlin from `peppy-mobile-bindings` belongs under `app/src/main/java`; generate it with the [bindings guide](../../crates/mobile-bindings/README.md). The Android build needs JDK 17, command-line tools, `platform-tools`, `platforms;android-36`, `build-tools;35.0.0`, and `ndk;27.2.12479018`. Set `JAVA_HOME` to JDK 17 and `ANDROID_HOME` or `ANDROID_SDK_ROOT` to that SDK. The optional builder runs as `linux/amd64`, including on Apple Silicon.

The native verifier builds and checks both `aarch64-linux-android` (`arm64-v8a`) and `x86_64-linux-android` (`x86_64`) libraries. It checks crypto symbols before Gradle packages either ABI.

```sh
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/27.2.12479018"
infra/compose/verify-android-native.sh
cd apps/android
./gradlew :jvm-smoke:run :app:testDebugUnitTest :app:lintDebug :app:assembleDebug :app:assembleDebugAndroidTest
```

The container builder requires `PEPPY_ACCEPT_ANDROID_LICENSES=1` before it accepts SDK licenses at runtime; no image build accepts them. With a selected emulator, the canonical workflow's `just android-smoke` installs and runs the instrumentation APK. It requires a positive test count and `INSTRUMENTATION_CODE: -1`. The instrumentation smoke covers generated bindings, SQLCipher/crypto, capture, reopen, and typed closed-handle errors. It does not validate live carrier SMS, MMS, RCS, or physical-network behavior.
