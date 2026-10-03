# OpenPush

[![CI](https://github.com/mattv8/openpush/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/mattv8/openpush/actions/workflows/ci.yml)

OpenPush is a self-hosted messaging foundation for developer and operator evaluation. It includes an encrypted-envelope server, encrypted attachment storage, snapshots, explicit public image copies, a simulated gateway, native desktop client, Android SMS companion, and capability-gated Swift client.

## Status

Development remains in progress. The simulator exercises synthetic messages, not a carrier. CI builds development artifacts on each push to `main` and publishes prerelease artifacts automatically. For stable releases, use the "Release" workflow with inputs for version bump, explicit version override, or a `dry_run` test. Download prerelease and CI artifacts from [GitHub Releases](https://github.com/mattv8/openpush/releases) and [CI runs](https://github.com/mattv8/openpush/actions/workflows/ci.yml). All artifacts provide build evidence only:

- Android release APK/AAB artifacts are unsigned.
- macOS bundles and DMGs, Windows installers, and Linux packages are development outputs. macOS bundles are development bundles, not Developer ID signed or notarized; macOS may apply ad-hoc linker signing without a TeamIdentifier or sealed resources.
- The iOS simulator app and unsigned device archive do not establish signing, store, or device eligibility. The archive is not an installable IPA.

The desktop provides unread/tray updates, native notification banners and an in-app feed for mirrored Android notifications. Android emulators can receive synthetic SMS. iOS has no carrier executor or third-party notification listener in this build. Native banner delivery depends on OS permission, system notification settings and platform installation requirements; browser previews cannot verify it.

## Components

- **Server:** device authentication, ordered encrypted envelopes, encrypted attachments, snapshots, and public image-copy endpoints.
- **Gateway simulator:** synthetic SMS and carrier-effect exercise for local development.
- **Native clients:** Tauri desktop, Android SMS companion with opt-in experimental MMS, and capability-gated Swift client.

Read the [Android](apps/android/README.md), [iOS](apps/ios/README.md), [mobile bindings](crates/mobile-bindings/README.md), and [desktop](apps/desktop/README.md) guides for component limits and native details.

## Contacts

Android and iOS can publish their address books to the encrypted vault. Enable
contact sync on each phone; each phone owns its book and applies edit requests
from other devices. The desktop Contacts view browses and edits those books.
New-contact destination and Auto / Confirm / Off edit policy are configured on
the owning phone. Large deletions require approval there.

Contacts include structured names, nickname, labeled phones/emails,
organization/title, postal addresses, birthday, Android notes and normalized
avatars. Photos are private encrypted 256×256 JPEG attachments, at most 64 KiB.
Deleted contacts retain restore data for 90 days; restoring requests a new OS
contact on the owning phone. Shared numbers remain distinct contacts, and
ambiguous name matches fall back to the address.

Contact writes require a request from the phone's active encryption epoch.
Rotating keys rejects older requests that have not received a write permit;
already-issued uncertain attempts remain reconcilable and are never reissued.

Phone permissions, locked keys and OS scheduling affect freshness. Android uses
bounded WorkManager passes; iOS also uses discretionary background refresh and
processing. Neither promises immediate remote edits. See the platform guides for
field restrictions and verification limits.

## Notification mirroring

Upgrade all participating clients before enabling mirroring on Android. Older builds quarantine unfamiliar notification records and do not retry them automatically after upgrade.

Mirroring requires Android notification access and an enabled mirroring switch. Apps are allowed by default; per-phone app filters can be changed on the companion or desktop. OpenPush's own notifications, the default SMS app's duplicate notifications, group summaries, ongoing/progress notifications and empty notifications are excluded. Locked sync does not collect a plaintext notification backlog.

The desktop's Notifications view provides the feed, app mute controls and dismissal. Phone dismissals propagate through sync. Desktop dismissal requests reach the phone at its next sync; Android background scheduling can delay this by 15 minutes or longer. This is not an immediate remote-control channel. Group summaries may remain on the phone after their children are dismissed.

The feed shows up to 1,000 active notifications, newest first; bulk dismissal handles up to 100 per action. The Android companion's **Refresh app list** reloads observed apps and synchronized filter choices.

Desktop Settings controls message banners, mirrored-notification banners, and full or hidden banner previews. These preferences are local to each desktop. Use the in-app feed for navigation and dismissal; native banner activation and notification-center interactions vary by OS.

### Storage and trust boundaries

Notification titles, text and app metadata travel inside encrypted envelopes. The server's replay log defaults to 30 days. Compatible clients can explicitly supersede notification state for compaction after it leaves replay retention; dismissal does not immediately erase previously synced ciphertext. Muting cannot recall an upload already in flight. Filters changed remotely take effect on the phone after it syncs.

Notification state shares the existing 100,000-record snapshot import limit. Coalescing and compaction reduce superseded state, but legacy records and immutable message history still consume storage and bootstrap capacity. Compaction does not remove this vault-wide limit.

Every passphrase holder retains the same authority within the vault, including the ability to request a phone notification's dismissal. Notification mirroring does not change carrier SMS/MMS encryption or add forward secrecy.

### Compaction and recovery

The server can compact a vault only after every non-revoked device declares
support for generation-fenced snapshots. Upgrade participating clients together;
an older device blocks compaction. Declaring support is a build-level promise:
downgrading a declared client can make that client unable to restore a compacted
snapshot.

Contact producers require a compatible server and the active epoch's compaction
key. An upgraded device with an older cached-key format must unlock once with
the shared passphrase. Existing message decryption can still work while contact
sync waits for that unlock. Local history is indexed in bounded passes before
contact production resumes. Missing historical epoch keys also pause contact
production: unlocking the current epoch cannot recover lost older keys. Keep
historical key material needed to read retained history. A device that has not declared snapshot support
pauses vault-wide cleanup even when the other devices can sync contacts.

Compaction follows explicit references to superseded records, rather than treating
the last upload as the latest state. A change to the retained snapshot set advances
its generation, causing an affected import to restart. Carrier commands are not
compaction targets. Unmarked legacy records are retained unless an eligible
replacement explicitly supersedes them.

Compaction exposes opaque grouping keys, supersession relationships and deletion
timing to the server. Reference-tracked private attachments additionally expose
their association with opaque envelope identities so the server can check whether
reclamation is safe. These metadata do not contain contact fields, photo plaintext
or encryption keys. Public image copies remain separate plaintext objects with
their own lifecycle.

Device bearer credentials authorize storage writes and retention operations.
Encryption protects contents; it does not protect ciphertext availability from
a compromised authorized credential. Revocation and operator backups remain
separate controls. Superseded ciphertext is not removed immediately, and retained
duplicate-detection identities continue to consume server storage after compaction.

## Security boundaries

Each vault uses a manually shared passphrase. Clients derive vault keys locally; the server stores and orders opaque envelopes and never receives that passphrase. The protocol does not claim forward secrecy. The encrypted vault-check header permits offline passphrase guessing; TLS and server rate limits cannot prevent it. Choose a long random multiword passphrase.

Every passphrase holder has the same cryptographic authority. Credential revocation does not remove access from a passphrase holder. Rotate the passphrase, activate the new epoch on each participating client, and keep historical epoch material needed to read history. A gateway blocks commands from a retired epoch. The composed protocol has not received an external security audit.

SMS and MMS move in plaintext outside OpenPush's encryption boundary. A gateway records carrier attempts durably and does not resend an attempt with an unknown carrier outcome. Snapshot history cannot execute carrier work. Copying a client database back behind the application is not detectable. Public attachment copies are separately supplied PNG, JPEG, or WebP plaintext derivatives. Their share tokens and expiry/revocation controls do not encrypt them; never upload private originals as public copies.

## Local server and TLS experiments

The local API publishes on `127.0.0.1:7000`; PostgreSQL and SeaweedFS have no host ports. `/healthz` reports whether the process can serve requests. `/readyz` also checks PostgreSQL, migrations, and, when configured, a bounded attachment-store probe. `just smoke-infra` starts the stack, checks both routes, and runs the storage contract check.

To use another local API port, keep all three override values aligned:

If your `.env` predates the 7000 default, update `PUBLIC_API_URL` and `PUBLIC_ATTACHMENT_URL` (and `API_HOST_PORT`, if set) to the same port, or set `API_HOST_PORT=8080` to keep the old port.

```sh
export API_HOST_PORT=18080
export PUBLIC_API_URL=http://127.0.0.1:18080
export PUBLIC_ATTACHMENT_URL=http://127.0.0.1:18080
docker compose --env-file .env -f docker-compose.yml up
```

For a public TLS experiment, set `PUBLIC_HOST` and add `-f docker-compose.caddy.yml` to the Compose command. Keep `PUBLIC_API_URL` and `PUBLIC_ATTACHMENT_URL` as external HTTPS origins, and keep `S3_INTERNAL_ENDPOINT` internal. The Caddy overlay is the only supplied configuration that publishes ports 80 and 443. It is not production-readiness evidence.

## Develop and contribute

Start with [CONTRIBUTING.md](CONTRIBUTING.md) for container, Android, and native desktop workflows.

Operators must read [backup and restore guidance](infra/compose/README.md) before recovery. A restore rewinds revocations and cursors and requires reconciliation and client resynchronization before service access resumes.

## Release boundary

Releases are git tags; checked-in manifests keep development placeholder versions and CI stamps the computed version into each build.

- **Prereleases:** when CI passes for a push to `main`, it publishes the GitHub prerelease `vX.Y.Z-main.N`, where `X.Y.Z` is the next version predicted from Conventional Commits and `N` counts commits since the last stable tag. Only the latest green run publishes; queued runs superseded by a newer push are skipped. The newest 10 prereleases are kept. The server image is pushed to `ghcr.io/mattv8/openpush-server` as `X.Y.Z-main.N` and `edge`.
- **Stable releases:** run the **Release** workflow from `main`. Inputs: `bump` (`auto`, `patch`, `minor`, `major`), an optional explicit `version` (`X.Y.Z`, for example `1.0.0`), and `dry_run` (build without publishing). The workflow requires a successful CI push run for the current `main` commit, rebuilds with the stable version, and creates the `vX.Y.Z` tag only when it publishes the release. Images are tagged `X.Y.Z`, `X.Y`, `latest`, and `X` from `1.0.0`.
- **Assets:** unsigned Android `openpush-<version>-android-unsigned.apk` and `.aab`; Linux AppImage, deb, and rpm (stable only); macOS DMG and app archive; Windows MSI and NSIS installer; `SHA256SUMS`; generated release notes. iOS artifacts stay in CI runs and are not published.
- On Windows, uninstall a prerelease MSI before installing the stable MSI of the same `X.Y.Z`.

Releases do not sign, notarize, publish to stores, or turn unsigned artifacts into signed releases. Store readiness additionally requires the appropriate Apple, Windows, Android, update-signing, privacy, policy, recovery, and review work. Keep signing credentials only in protected CI environment secrets; never generate or commit them in this repository.

Preview the next version with `just version --channel prerelease`. Run `just release-test` after changing `infra/release/` or the release workflows; it needs git-cliff 2.14.2 on `PATH`, which `infra/release/install-git-cliff.sh <dir>` installs.
