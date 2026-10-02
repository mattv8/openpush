# OpenPush Android companion shell

Follow the canonical [contributing workflow](../../CONTRIBUTING.md) for setup, container builds, emulator operations, and checks.

This Kotlin/Compose companion captures new SMS broadcasts and can submit permitted SMS commands through Android's carrier API. It is not the default SMS app: it requests `RECEIVE_SMS` and `SEND_SMS`, never `WRITE_SMS`, and has no `READ_SMS` permission or inbox-history scan.

SMS uses the current default SMS subscription. If that subscription changes or disappears, a command for the old route remains pending and does not move to another SIM. MMS is unsupported and does not downgrade to SMS; ordinary apps have no general RCS inbox or send API. The shell does not request the default-SMS role, `WRITE_SMS`, hidden APIs, or unverified RCS access. A physical carrier route needs granted SMS permissions and a current default SMS SIM. Emulator and unit results do not prove carrier or store behavior.

Generated Kotlin from `openpush-mobile-bindings` belongs under `app/src/main/java`; generate it with the [bindings guide](../../crates/mobile-bindings/README.md). The Android build needs JDK 17, command-line tools, `platform-tools`, `platforms;android-36`, `build-tools;35.0.0`, and `ndk;27.2.12479018`. Set `JAVA_HOME` to JDK 17 and `ANDROID_HOME` or `ANDROID_SDK_ROOT` to that SDK. The optional builder runs as `linux/amd64`, including on Apple Silicon.

The native verifier builds and checks both `aarch64-linux-android` (`arm64-v8a`) and `x86_64-linux-android` (`x86_64`) libraries. It checks crypto symbols before Gradle packages either ABI.

```sh
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/27.2.12479018"
infra/compose/verify-android-native.sh
cd apps/android
./gradlew :jvm-smoke:run :app:testDebugUnitTest :app:lintDebug :app:assembleDebug :app:assembleDebugAndroidTest
```

The container builder requires `OPENPUSH_ACCEPT_ANDROID_LICENSES=1` before it accepts SDK licenses at runtime; no image build accepts them. With a selected emulator, the canonical workflow's `just android-smoke` installs and runs the instrumentation APK. It requires a positive test count and `INSTRUMENTATION_CODE: -1`. The instrumentation smoke covers generated bindings, SQLCipher/crypto, capture, reopen, and typed closed-handle errors. It does not validate live carrier SMS, MMS, RCS, or physical-network behavior.
