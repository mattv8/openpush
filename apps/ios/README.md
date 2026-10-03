# Peppy iOS gateway client

Follow the canonical [contributing workflow](../../CONTRIBUTING.md) for shared setup and checks.

This SwiftUI app is a product **gateway-role client** with Mirroring, SMS, and Account tabs plus Settings. It can enroll, unlock, synchronize, manage its device account, contacts, and optional wake enrollment. It is not a carrier executor: iOS has no carrier SMS/MMS executor or inbox listener in this build, no system-wide notification listener, and no RCS integration. Subscriber conversation, composer, and feed UI remain the unchanged shared webview surface.

## Enrollment, roles, and local security

Pair by scanning the desktop's owner-approved QR intent or import a v1 credential file. The QR payload is JSON with exactly `https_origin` and `intent_token`; it never carries a passphrase, owner credential, vault metadata, or phone private key. The phone creates its own signing key in native secure storage, computes the SAS locally, waits for owner approval, signs the shared Rust pairing proof, and imports the consumed credential. The existing shared vault passphrase is entered manually in a `SecureField`, passed to the core once, and is never stored.

The Account tab uses the actual role returned by the server. A device can sign itself out; only an `owner` can remove another device and see the typed-`ERASE` vault-deletion control. Gateway capability or a SIM does not grant owner privileges. Disconnect archives the encrypted database, keys, and queued work locally; it is not destructive reset or proof that a revoked credential can resume sync. Keychain records are device-only and not synchronized or restored by backup.

## Shared policy and iOS capability limits

Gateway settings and policy are durable shared Rust-core state accessed through generated UniFFI bindings. iOS supplies host facts only; it does not duplicate policy persistence. The UI reports SMS/MMS capture and notification mirroring as unavailable because this build has no carrier executor, SMS/MMS listener, or notification-listener API. RCS is unavailable. Contacts and administrative gateway functions remain available.

Foreground sync is bounded and cancels when the scene leaves the foreground. Contact passes may also request `BGAppRefresh` and `BGProcessing`, but iOS chooses whether and when to run them; requested starts are not delivery guarantees. Host tests use fakes and do not establish device Contacts, background scheduling, or iOS SDK behavior.

## Optional APNs wake relay

The APNs wake relay is off until the user explicitly configures an operator HTTPS relay in Settings. APNs registration is then requested, but wake delivery is not guaranteed and no live signed APNs delivery is claimed here. Hints carry only the relay's nested `peppy` `kind` and optional challenge value; they contain no message body. A received wake starts bounded sync/contact work and never authorizes carrier work. If a hint is delayed or missing, authoritative queued sync remains available on later foreground work.

Vault sync envelopes and private attachments keep the existing encrypted body transport. Carrier SMS/MMS would be outside that boundary, but iOS does not perform carrier transport in this build.

## Layout

- `Generated/`: UniFFI output owned by `crates/mobile-bindings`; never edit it by hand.
- `PeppyNative/`: enrollment, Keychain, bounded sync, contacts, gateway capability, and relay host code over the generated core facade.
- `ContactsHistory/`: Objective-C bridge for Contacts change history.
- `PeppyMobile/`: the SwiftUI app and iOS-only push wiring.
- `Smoke/` and `Tests/PeppyNativeTests/`: host smoke and Swift Testing coverage.

## Generate and verify

Generate Swift bindings from the repository root, then run host checks from this directory. With Command Line Tools only, Swift Testing requires its explicit macro-plugin path:

```sh
cargo build --locked -p peppy-mobile-bindings
cargo run --locked -p peppy-mobile-bindings --features cli --bin uniffi-bindgen -- generate --library target/debug/libpeppy_mobile_bindings.dylib --language swift --out-dir apps/ios/Generated

cd apps/ios
swift build
swift run PeppyMobileSmoke
swift test -Xswiftc -plugin-path -Xswiftc /Library/Developer/CommandLineTools/usr/lib/swift/host/plugins/testing
```

`swift build` and `swift test` compile macOS SwiftPM targets only; they are not evidence that the iOS app compiles. An iOS build requires full Xcode, an iOS SDK, and the Rust static library for `aarch64-apple-ios` or `aarch64-apple-ios-sim` at the project's configured library-search paths. Local Command Line Tools cannot substitute for that SDK verification.
