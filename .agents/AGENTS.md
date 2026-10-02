# OpenPush domain guidance

This reference records durable repository constraints. For desktop React work,
also use [the frontend style guide](frontend-style-guide.md).

## Ownership boundaries

- Rust core is the sole owner of local SQLite/SQLCipher and durable
  domain/protocol state. Native hosts own transport, OS scheduling, secure
  storage, cancellation, and carrier effects. Web UI receives sanitized DTOs,
  never passphrases, device credentials, database keys, or encryption keys.
  See [client core](../crates/client-core/src/lib.rs) and
  [desktop sessions](../apps/desktop/src-tauri/src/session.rs).
- Device sync uses a manually shared passphrase. Do not substitute automatic
  key distribution or claim forward secrecy. Carrier SMS/MMS is outside the
  application's encryption boundary.
- Cached purpose keys can restore a previously verified, unlocked epoch but do
  not pin a profile or change the active epoch; they can make an already-active
  epoch usable. Only manual passphrase/header unlock records cache integrity,
  and hosts explicitly cut over to a newer verified epoch after that unlock.
  See [key lifecycle](../crates/client-core/src/lib.rs) and [desktop unlock](../apps/desktop/src-tauri/src/lib.rs).
- A published attachment is a separately supplied plaintext derivative, not a
  publication of the encrypted original. See
  [attachment API](../services/server/src/api/attachments.rs).

## Commands, delivery, and recovery

- Keep local durable receipt, transport acknowledgement, application state,
  carrier submission, and delivery evidence distinct. `SendState` defines
  which states may retry transport or carrier work; `OutcomeUnknown` requires
  reconciliation and must not be automatically resent. See
  [send states](../crates/domain/src/command.rs) and
  [retry policy](../crates/sync-core/src/retry.rs).
- A gateway receives a durable, one-use carrier permit only for a command in
  its received-command ledger. An unfinished attempt reopens as
  `OutcomeUnknown`, never with a second permit. See
  [client-core contract](../crates/client-core/src/lib.rs).
- The receive cursor is the highest contiguous journaled cursor; quarantine
  malformed records rather than wedging the stream. Snapshot import is staged
  and monotonic. Snapshot commands are historical and never authorize carrier
  work. A restore guard persists before import validation, so do not claim a
  rejected restore leaves all state unchanged.
- Server replay is retained transport history, while snapshots are immutable
  ciphertext records for resync. See [sync API](../services/server/src/api/sync.rs).

## Protocol contracts

- Rust is the source of shared wire contracts. Generate bindings and schemas
  from it rather than maintaining equivalent hand-written platform models.
  [Envelope generation](../crates/protocol/src/generate.rs) covers envelope
  and pairing schemas, not every REST adapter.
- Preserve distinct identity roles: envelope, command, device, vault, event,
  message, and source sequence are not interchangeable. See
  [domain IDs](../crates/domain/src/ids.rs) and
  [envelope headers](../crates/protocol/src/envelope.rs).
- `Cursor` and `SourceSequence` serialize as canonical unsigned decimal
  strings, not JSON numbers or padded strings. Envelope header/ciphertext
  validation is a contract boundary.

## Notifications

- While vault keys are locked, notification plaintext is dropped: there is no
  plaintext locked queue. SMS capture has different durable locked handling;
  do not generalize notification behavior to it. See
  [notification capture](../crates/client-core/src/notifications.rs) and
  [client-core contract](../crates/client-core/src/lib.rs).
- Notification identity includes source device, notification key, and lifetime;
  a current OS instance tracks updates/removals. Android guards removal with
  the same key and `postTime` instance, not the key alone.

## Android carrier companion

- Android is a companion to the default messaging app: do not request the
  default-SMS role or `WRITE_SMS`, use hidden APIs, or imply general RCS
  support. Default-app status alone does not grant RCS access. See
  [Android limits](../apps/android/README.md).
- MMS capture reconciles content already downloaded by the default messaging
  app's provider; it does not make OpenPush the carrier/default app. Provider
  acquisition is offline reconciliation, separate from gateway upload. See
  [MMS capture](../apps/android/app/src/main/java/dev/openpush/mobile/MmsCaptureWork.kt).
- A SIM route is the exact current default-SMS subscription route, not a
  physical-SIM identity. Commands for an old or missing route remain pending;
  never redirect them to another SIM. See
  [SIM routes](../apps/android/app/src/main/java/dev/openpush/mobile/SimRoutes.kt).

## iOS limits

- This build has no carrier executor and reports carrier messaging unavailable;
  it never calls `begin_send_attempt` on iOS. Sync is foreground-only, bounded,
  and cancelled on leaving foreground—there is no background task or long-lived
  WebSocket. See [iOS behavior](../apps/ios/README.md) and
  [telephony eligibility](../apps/ios/OpenPushNative/TelephonyEligibility.swift).
- A host Swift package build is not evidence of an iOS SDK build, carrier
  eligibility, or carrier capability.

## Desktop native boundary

- A missing protected database key must fail closed when the local database
  already exists; never reset or replace that database. Database keys are bound
  to a vault/device identity and generated only for an absent database. See
  [credential handling](../apps/desktop/src-tauri/src/credentials.rs).
- Desktop secure storage fails closed when unavailable. The bundled macOS
  secure-store record preserves compatible legacy entries and rejects malformed
  records or conflicting database keys rather than replacing them. See
  [secure store](../apps/desktop/src-tauri/src/secure_store.rs).
- Keep passphrases in reviewed native secure controls, outside the webview,
  Tauri IPC, and process argv. The desktop dialog implementation covers macOS,
  Windows, and Linux native paths. See
  [native dialogs](../apps/desktop/src-tauri/src/dialogs.rs).
- Windows uses a nonpersisting native CredUI prompt. Linux tries `zenity`, then
  `kdialog`, through `Command` without a shell; absent helpers fail closed with
  no webview fallback.
- Native floating-head click-through needs real input-region/focus evidence;
  CSS rounding or webview hit testing does not establish it. Preserve a usable
  main-window/composer fallback.

## Workspace and evidence boundaries

- The root Rust workspace excludes `apps/desktop/src-tauri`; treat its build
  and dependency graph as separate. See [root Cargo workspace](../Cargo.toml).
- Contract coverage and integration fixtures are documented in
  [CONTRIBUTING.md](../CONTRIBUTING.md): generated contracts do not cover every
  REST adapter, and database/S3 integration fixtures are fail-fast and distinct
  from persistent development volumes.
- Keep simulator, native-build, real-carrier, store-distributable, and
  production-readiness evidence separate. Android carrier and iOS capability
  claims require the applicable evidence, not a fixture or host build.
