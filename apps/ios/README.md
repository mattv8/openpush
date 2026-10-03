# OpenPush iOS client foundation

Follow the canonical [contributing workflow](../../CONTRIBUTING.md) for shared setup and checks. This guide retains iOS-specific limits and host commands.

The iOS app is a native SwiftUI client. It is **not** a carrier gateway: iOS carrier
sending stays unavailable in this build (see "Carrier messaging" below).

## Layout

- `Generated/`: UniFFI output owned by `crates/mobile-bindings`. Never edit it by hand.
- `OpenPushNative/`: Foundation/Security code shared by the app and the macOS SwiftPM
  build. It covers credential import, Keychain storage, the session, bounded sync,
  contact capture and edits, and the telephony gate. Durable state goes through the generated core facade.
- `ContactsHistory/`: Objective-C bridge for the Contacts change-history API that
  is unavailable directly from Swift.
- `OpenPushMobile/`: the SwiftUI app (one `Form`), compiled only by the Xcode project.
- `Smoke/`: the generated Swift → Rust SQLCipher smoke.
- `Tests/OpenPushNativeTests/`: Swift Testing tests. They use real core clients,
  an in-memory secure store, and a local fake of the server's JSON contract.

## Behavior

- **Import.** The app takes the strict v1 credential file written by operator or
  simulator pairing: `version`, `origin`, `vaultId`, `deviceId`, and a 96-hex
  `deviceToken`.
  - The origin must be canonical HTTPS. Plain HTTP is allowed only for loopback in
    debug builds.
  - Before anything is stored, the app calls `GET /v1/vault` with the token. The
    server must report the same vault and device.
- **Keychain.** Items are generic passwords with
  `AfterFirstUnlockThisDeviceOnly` and are never synchronized. Each account is
  bound to its purpose, origin, vault, and device. The Keychain holds:
  - the device token
  - the 32-byte SQLCipher key, which is add-only and never overwritten
  - the public profile and encrypted header
  - one opaque core key cache per epoch

  No backup or reinstall restoration is claimed. The database directory is excluded
  from backup.
- **Refresh, disconnect, and reinstall.**
  - *Refresh* re-reads `/v1/vault` with the stored token.
  - *Replace credential* re-imports the same origin, vault, and device.
  - *Disconnect* asks for confirmation, then removes only the active pointer. The
    encrypted database, its key, key caches, token, and unsent outbox stay archived,
    and re-importing the same credential reopens them.
  - If this identity's database once existed but is gone (for example after a
    reinstall that kept the Keychain), the app fails closed. It never recreates that
    database. Disconnect, then pair as a new device.
  - A database file without its key is never replaced.
  - There is no destructive reset.
- **Unlock.** The user enters the vault's existing shared passphrase in a
  `SecureField`. It goes to the core once and is never stored. Only this manual
  unlock activates the server-verified key epoch (`activateVerifiedEpoch`); import,
  refresh, and key-cache restore never do. An open database does not mean the vault
  keys are unlocked.
- **Sync.** Sync runs one bounded pass each time the scene becomes active.
  - One request budget and one apply budget cover the whole pass, so repeated
    resyncs stay bounded.
  - Each pass sends a bounded outbox batch (`pendingOutboxJsonBatch`) and
    acknowledges each envelope only after the server accepts it.
  - If the server refuses an envelope, it stays queued and receive still runs.
  - A 401, a cancellation, or an origin or redirect violation stops the pass.
  - Then the pass finishes a staged snapshot and drains published snapshot
    records before any live replay.
  - Redirects are refused, responses must come from the same origin, and response
    bodies are capped.
  - Leaving the foreground cancels the pass.
   - Contact sync also schedules bounded background tasks. There is no long-lived WebSocket.
- **Carrier messaging.** `TelephonyEligibility` is derived from what the build
  contains. There is no entitlement and no executor, and default-app and EU
  eligibility are not verified, so it always reports unavailable. Core commands
  are only counted; `begin_send_attempt` is never called on iOS.

## Contact sync

Enable **Sync this phone's contacts…** after unlocking the vault, then grant
Contacts access. The phone publishes its own book; other devices send edit
requests for this phone to apply. Limited access publishes only visible contacts
and never treats invisible contacts as deleted.

The Contacts section selects the destination account for new contacts and the
remote-edit policy: **Auto** (default), **Confirm**, or **Off**. Auto still holds
large deletions for approval. Pending approvals appear on the phone. **Pause**
leaves the last published book intact; **Retire** marks it retired without
deleting OS contacts. Re-enabling after retirement starts a new book.

Names, nickname, labeled phones and emails, organization/title, postal addresses,
birthday and normalized contact photos sync. iOS notes are excluded because this
build has no contacts-notes entitlement. Photos become private encrypted 256×256
JPEG attachments, at most 64 KiB; original full-resolution photos are not replicated.

Foreground and contact-change passes share a coordinator with `BGAppRefresh` and
`BGProcessing` passes. Expiration cancels work; scan and photo progress resume
from the encrypted core. Change-history additions and updates use an incremental
path; deletions, unavailable history and oversized batches trigger a bounded full
scan. Permission loss and incomplete scans never infer deletions. Interrupted OS
writes are reconciled rather than automatically repeated.

iOS chooses when background tasks run. The requested earliest starts (30 minutes
for refresh and six hours for processing) are not delivery intervals or deadlines.
Updates may take hours or days. This build has no APNs wake integration. Host tests
use a fake contact store and HTTP server; they do not establish real-device
Contacts, background scheduling or iOS SDK behavior.

## Commands (macOS, Command Line Tools)

Run these from this directory after building `openpush-mobile-bindings`. With
Command Line Tools only, the Swift Testing macro plugin path must be passed
explicitly.

```sh
swift build
swift run OpenPushMobileSmoke
swift test -Xswiftc -plugin-path -Xswiftc /Library/Developer/CommandLineTools/usr/lib/swift/host/plugins/testing
```

`swift build` builds macOS targets only. Building the iOS app needs full Xcode, an
iOS SDK, and the Rust static library built for `aarch64-apple-ios` or
`aarch64-apple-ios-sim`. The project's `LIBRARY_SEARCH_PATHS` point at those
target directories.
