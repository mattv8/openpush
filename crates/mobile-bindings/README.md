# OpenPush mobile bindings

`openpush-mobile-bindings` is the UniFFI 0.32.2 facade over the shared
SQLCipher client. Native hosts own transport, scheduling, carrier effects, and
secure persistence; the binding does not provide an HTTP client, callbacks, or
an implicit runtime.

## ABI

- `open_native_client(NativeOpenConfig)` accepts a native-supplied 32-byte
  database key. `unlock(profile_json, header_json, passphrase)` receives the
  shared passphrase only for unlock; hosts must not put it in view models or
  persistent ordinary storage.
- `export_native_key_cache_for_native_storage` and
  `import_native_key_cache_from_native_storage` exchange opaque bytes for
  Android Keystore, iOS Keychain, or equivalent native secure storage. Message
  view records never return database, passphrase, purpose, or envelope keys.
- Native sync workers send opaque envelope JSON from
  `pending_outbox_json_batch(limit)` and call `ack_outbox` only after acceptance.
  A nonzero batch seals a bounded number of pending rows and returns at most
  500 sealed envelopes; it neither activates an epoch nor performs I/O.
- `ingest_raw`, `apply_pending`, and the snapshot methods support bounded
  journal/apply work. After `finish_snapshot`, keep calling `apply_pending`
  until `snapshot_remaining` is zero.
- `activate_verified_epoch(epoch)` is an explicit manual cutover after a
  successful unlock of that verified profile. Credential import and key-cache
  restoration do not activate an epoch.
- `dispose()` rejects later calls with `MobileBindingsError.Closed`, while work
  admitted before disposal retains its core handle and may finish.

Generate host Kotlin and Swift bindings with the repository-local pinned tool:

```sh
cargo build --locked -p openpush-mobile-bindings
cargo run --locked -p openpush-mobile-bindings --features cli --bin uniffi-bindgen -- generate --library target/debug/libopenpush_mobile_bindings.dylib --language kotlin --out-dir apps/android/app/src/main/java
cargo run --locked -p openpush-mobile-bindings --features cli --bin uniffi-bindgen -- generate --library target/debug/libopenpush_mobile_bindings.dylib --language swift --out-dir apps/ios/Generated
```

Use the corresponding library under `target/release` for release generation.
For Android, `infra/compose/verify-android-native.sh` performs host generation,
builds the ARM64 library, verifies crypto symbols, and copies it into `jniLibs`.
Generated bindings are Rust-owned output, not hand-maintained protocol models.
