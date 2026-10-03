# Peppy mobile bindings

Follow the canonical [contributing workflow](../../CONTRIBUTING.md) for shared setup and checks.

`peppy-mobile-bindings` is the UniFFI 0.32.2 facade over the shared SQLCipher client and Rust-owned gateway policy. Native hosts provide HTTP transport, OS scheduling, carrier effects, current OS facts, and secure persistence; the binding provides no implicit HTTP client, callbacks, or runtime.

## Native boundary

- `open_native_client(NativeOpenConfig)` receives a native-supplied 32-byte database key. `unlock(profile_json, header_json, passphrase)` accepts the shared vault passphrase only for manual unlock; hosts must not retain it in ordinary storage or view models.
- `export_native_key_cache_for_native_storage` and `import_native_key_cache_from_native_storage` exchange opaque bytes with Android Keystore, iOS Keychain, or an equivalent native store. Views never receive database, passphrase, purpose, or envelope keys.
- `pending_outbox_json_batch(limit)` returns opaque encrypted envelope JSON. Hosts call `ack_outbox` only after server acceptance. It performs neither I/O nor epoch activation.
- `ingest_raw`, `apply_pending`, and snapshot methods support bounded journal/apply work. After `finish_snapshot`, call `apply_pending` until `snapshot_remaining` is zero.
- `activate_verified_epoch(epoch)` is explicit after a successful manual unlock of that verified profile. Credential import and key-cache restoration do not activate an epoch.
- `dispose()` rejects later calls with `MobileBindingsError.Closed`; already-admitted work retains its core handle and may finish.

## Gateway policy, capabilities, and pairing

`NativeGatewaySettings` stores shared durable preferences: mirroring, Wi-Fi-only mirroring, skip-silent, SMS/MMS synchronization, and Wi-Fi-only media. `gateway_policy_decision` combines those preferences with host-supplied `NativeGatewayHostFacts`; native code must not create a parallel policy store. New mirroring defaults off. Capability and policy results are distinct: a host must report actual permission/listener/Wi-Fi facts and must enforce the resulting decision at the OS effect boundary.

`NativeGatewayPlatform` and `gateway_capabilities` keep platform limits explicit. iOS hosts report no carrier executor or notification listener in this product build; Android must not infer RCS or privileged carrier access from SIM/default-app state.

Pairing helpers produce Rust/protocol-owned canonical proof bytes and SAS-related material. Native hosts own their signing keys. The QR contract belongs to the enrollment host: it is JSON containing exactly `https_origin` and `intent_token`; it must not carry credentials, passphrases, vault metadata, or private key material. Hosts retain the existing manual passphrase-unlock boundary.

The binding handles encrypted vault records only. Relay providers and carrier transport are host/service boundaries: content-free wake hints do not authorize carrier work, and carrier SMS/MMS bodies remain outside Peppy's encrypted transport boundary.

## Generate bindings

Use the repository-local pinned generator. Generated Kotlin and Swift are Rust-owned output, not hand-maintained protocol models:

```sh
cargo build --locked -p peppy-mobile-bindings
cargo run --locked -p peppy-mobile-bindings --features cli --bin uniffi-bindgen -- generate --library target/debug/libpeppy_mobile_bindings.dylib --language kotlin --out-dir apps/android/app/src/main/java
cargo run --locked -p peppy-mobile-bindings --features cli --bin uniffi-bindgen -- generate --library target/debug/libpeppy_mobile_bindings.dylib --language swift --out-dir apps/ios/Generated
```

Use the corresponding library under `target/release` for release generation. For Android, `infra/compose/verify-android-native.sh` generates bindings, builds `arm64-v8a` and `x86_64` libraries, verifies crypto symbols, and copies them into `jniLibs`. For iOS, host SwiftPM checks do not compile iOS SDK targets; use full Xcode and the matching Rust iOS static library for SDK verification.
