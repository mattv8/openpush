# OpenPush

OpenPush is an in-progress, self-hosted messaging foundation for developer and operator evaluation. The server authenticates devices, stores and orders encrypted protocol envelopes, supports encrypted attachment storage, snapshots, and explicit public image copies. The repository includes a deliberately simulated gateway, a native desktop client, an Android SMS companion, and a capability-gated Swift client.

Physical-carrier, production, and store qualification remain incomplete. Do not treat the simulator as carrier proof, public copies as encrypted content, or a successful native build as a verified device workflow.

## Development builds

The [CI workflow](https://github.com/mattv8/openpush/actions/workflows/ci.yml) runs tests and builds development artifacts on pushes to `main`. Download artifacts from the completed run's **Artifacts** section. Green pushes to `main` also publish a [GitHub prerelease](https://github.com/mattv8/openpush/releases); stable releases come from the **Release** workflow, which can also build without publishing (`dry_run`). See [Release boundary](#release-boundary).

Artifacts include an installable Android debug APK, unsigned Android release APK/AAB, desktop bundles (macOS app/DMG, Windows installers, Linux packages), an iOS simulator app, and an unsigned iOS device archive. The unsigned device archive is not an installable IPA. Apple notarization, distribution signing and store uploads are separate from these development builds.

For local Android builds and JNI tests, see [apps/android/README.md](apps/android/README.md). The desktop can be bundled from the repository root with `pnpm --dir apps/desktop exec tauri build -- --locked`; its bundles are written under the desktop Cargo target's `release/bundle/` directory.

SMS captured by the Android companion can sync to the desktop. Desktop unread/tray updates are implemented; native OS notification banners are not yet implemented. An Android emulator can exercise simulated incoming SMS, while real carrier delivery requires a phone with the necessary permissions and SIM.

## Start the local server

Install the Rust toolchain in `rust-toolchain.toml`, Node from `.node-version`, pnpm 12.8.1, `just`, Docker Compose, and `curl`. Copy `.env.example` to `.env` and replace its values with synthetic development credentials. Do not commit `.env`.

On macOS, if the installed Homebrew Rust and Node 24 kegs are not on your path, select them for the current terminal:

```sh
export PATH="$(brew --prefix rustup)/bin:$(brew --prefix node@24)/bin:$PATH"
```

```sh
just doctor
just dev-up
curl --fail http://127.0.0.1:8080/healthz
curl --fail http://127.0.0.1:8080/readyz
just dev-down
```

`/healthz` reports whether the process can serve requests. `/readyz` additionally checks PostgreSQL, migrations, and, when configured, a bounded attachment-store probe. `just smoke-infra` starts the stack, checks both routes, and runs the storage contract check.

The API is published only on `127.0.0.1:8080`; PostgreSQL and SeaweedFS have no host ports. Named volumes retain PostgreSQL, SeaweedFS volume data, and SeaweedFS filer metadata. To change the local API port, keep the public origins in sync:

```sh
export API_HOST_PORT=18080
export PUBLIC_API_URL=http://127.0.0.1:18080
export PUBLIC_ATTACHMENT_URL=http://127.0.0.1:18080
docker compose --env-file .env -f docker-compose.yml up
```

For a public TLS experiment, set `PUBLIC_HOST` and add `-f docker-compose.caddy.yml` to the Compose command. Keep `PUBLIC_API_URL` and `PUBLIC_ATTACHMENT_URL` as external HTTPS origins, and keep `S3_INTERNAL_ENDPOINT` internal. The Caddy overlay is the only supplied configuration that publishes ports 80 and 443. It is not production-readiness evidence.

## What is protected—and what is not

Each vault uses a manually shared passphrase; OpenPush does not distribute it automatically and does not claim forward secrecy. Clients derive vault keys locally and verify the authenticated vault header. The server stores and orders opaque envelopes; it never receives the passphrase or derives the vault keys.

Stored ciphertext and the vault-check header permit offline passphrase guessing; TLS and server rate limits cannot prevent that. Choose a long, randomly selected multiword passphrase for non-test data. Every holder of that phrase has the same cryptographic authority, and credential revocation alone does not exclude a key holder—change the phrase and explicitly activate the new epoch on participating devices. The composed protocol has not received an external security audit.

SMS and MMS carrier transport are plaintext outside this application's encryption boundary. Native gateway code records carrier attempts durably and does not automatically resend an attempt whose carrier outcome is unknown. A snapshot imports history only: historical commands are not executable. A gateway restored through the supported restore flow is durably guarded from new carrier attempts until re-enrolled with a fresh identity and reconciled. Copying a client database back behind the application is not detectable.

Attachments are encrypted client-side before private storage. A public copy is different: the owner deliberately uploads a separately supplied PNG, JPEG, or WebP derivative for unauthenticated sharing. It is plaintext, subject to its share token and expiry/revocation, and is not the encrypted original. Do not upload private originals or sensitive content as public copies.

Passphrase rotation is a manual epoch cutover. A client can keep old epochs readable, but a gateway blocks commands from a retired epoch. Keep the required historical passphrase/profile material for any history that must remain readable; there is no automatic recovery without it.

## Try the simulated gateway

This local demo uses the tracked disposable PostgreSQL/S3 test stack from `infra/compose/test-env.sh`, then runs the server and simulated gateway as native Rust binaries. It applies current migrations without resetting or dropping data. The simulated carrier action remains a local effect log; this is not carrier, native keychain, password-dialog, APNs/FCM, store, or production evidence.

The commands require Docker Compose, the repository Rust toolchain with `cargo` on `PATH`, `curl`, and Python 3. They choose a free loopback API port instead of port 8080, keep each run under the ignored demo directory, and set a private umask before creating credentials or the SQLCipher database. The tracked infrastructure helper uses synthetic test-only credentials. Do not substitute real credentials.

```sh
set -eu
umask 077

DEMO=.opencode/sessions/messaging-foundation/demo
RUN="$DEMO/run-$(date +%Y%m%d-%H%M%S)"
GATEWAY="$RUN/gateway"
DESKTOP="$RUN/desktop"
mkdir -p "$GATEWAY" "$DESKTOP"
chmod 700 "$DEMO" "$RUN" "$GATEWAY" "$DESKTOP"

source infra/compose/test-env.sh
openpush_test_infra_up

PORT="$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(('127.0.0.1', 0))
    print(listener.getsockname()[1])
PY
)"
printf '%s\n' "$PORT" > "$RUN/server-port"

export BIND_ADDR="127.0.0.1:$PORT"
export PUBLIC_API_URL="http://127.0.0.1:$PORT"
export PUBLIC_ATTACHMENT_URL="$PUBLIC_API_URL"
export OPENPUSH_SIMULATOR_PASSPHRASE='developer-only demo phrase'

cargo run --locked --quiet -p openpush-server -- migrate
cargo build --locked --quiet -p openpush-server -p openpush-gateway-simulator
target/debug/openpush-server > "$RUN/server.log" 2>&1 &
SERVER_PID=$!
printf '%s\n' "$SERVER_PID" > "$RUN/server.pid"
curl --fail --retry 20 --retry-connrefused --retry-delay 0 "$PUBLIC_API_URL/readyz"

cargo run --locked --quiet -p openpush-gateway-simulator -- prepare-vault "$GATEWAY"
cp "$GATEWAY/vault-bootstrap.json" "$DESKTOP/vault-bootstrap.json"
chmod 600 "$DESKTOP/vault-bootstrap.json"
```

Create the owner credential without putting the passphrase, owner token, or header bytes in an argument or log. The Python process reads the public bootstrap, supplies the required owner fields only to the child process environment, captures `create-owner` standard output, and creates the native credential JSON with mode `0600`.

```sh
export RUN GATEWAY DESKTOP PUBLIC_API_URL
python3 - <<'PY'
import json, os, subprocess
from pathlib import Path

run = Path(os.environ['RUN'])
bootstrap = json.loads((Path(os.environ['GATEWAY']) / 'vault-bootstrap.json').read_text())
env = os.environ.copy()
env.update({
    'OPENPUSH_OWNER_PUBLIC_KEY_PROFILE': json.dumps(bootstrap['profile'], separators=(',', ':')),
    'OPENPUSH_OWNER_VAULT_CHECK_HEADER_HEX': json.dumps(
        bootstrap['header'], separators=(',', ':')
    ).encode().hex(),
    'OPENPUSH_OWNER_PROFILE_FINGERPRINT': bootstrap['fingerprint'],
    'OPENPUSH_OWNER_KEY_EPOCH': str(bootstrap['profile']['key_epoch']),
})
result = subprocess.run(
    ['target/debug/openpush-server', 'create-owner'],
    env=env,
    check=True,
    text=True,
    stdout=subprocess.PIPE,
)
fields = dict(line.split('=', 1) for line in result.stdout.splitlines())
credential = {
    'version': 1,
    'origin': os.environ['PUBLIC_API_URL'],
    'vaultId': fields['vault_id'],
    'deviceId': fields['device_id'],
    'deviceToken': fields['device_token'],
}
path = run / 'owner.json'
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w') as output:
    json.dump(credential, output, indent=2)
    output.write('\n')
print(f'owner credential written privately: {path}')
PY

cargo run --locked --quiet -p openpush-gateway-simulator -- \
  pair "$GATEWAY" "$RUN/owner.json" gateway
cargo run --locked --quiet -p openpush-gateway-simulator -- \
  pair "$DESKTOP" "$RUN/owner.json" device
```

The separate desktop directory contains only the copied public bootstrap before pairing, so gateway and desktop signing-key files cannot collide. The following controller sends one synthetic incoming SMS, waits for the simulator's normal sync cycle, quits, checks the real replay API through the paired desktop credential, and reopens the persisted client-core SQLCipher database. Tokens remain inside the Python process.

```sh
python3 - <<'PY'
import json, os, selectors, subprocess, time, urllib.request
from pathlib import Path

run = Path(os.environ['RUN'])
gateway = Path(os.environ['GATEWAY'])
log_path = run / 'simulator.log'
fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
log = os.fdopen(fd, 'w')
proc = subprocess.Popen(
    ['target/debug/openpush-gateway-simulator', 'run', str(gateway)],
    env=os.environ.copy(),
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    bufsize=1,
)
selector = selectors.DefaultSelector()
selector.register(proc.stdout, selectors.EVENT_READ, 'stdout')
selector.register(proc.stderr, selectors.EVENT_READ, 'stderr')

def wait_for(marker, source, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        events = selector.select(deadline - time.monotonic())
        if not events:
            break
        for key, _ in events:
            line = key.fileobj.readline()
            if not line:
                continue
            log.write(f'{key.data}: {line}')
            log.flush()
            if key.data == source and marker in line:
                return
    raise RuntimeError(f'timed out waiting for {source} marker {marker!r}')

wait_for('gateway running', 'stdout')
wait_for('phase=media', 'stderr')
proc.stdin.write(json.dumps({
    'type': 'incoming',
    'senderAddress': '+15555550123',
    'body': 'synthetic runnable demo message',
    'providerMessageId': 'demo-incoming-1',
}) + '\n')
proc.stdin.flush()
wait_for('phase=sync', 'stderr')
wait_for('phase=media', 'stderr')
wait_for('phase=sync', 'stderr')
proc.stdin.write('{"type":"quit"}\n')
proc.stdin.flush()
proc.wait(timeout=20)
log.close()
if proc.returncode != 0:
    raise SystemExit(f'simulator exited {proc.returncode}; inspect {log_path}')

credential = json.loads((Path(os.environ['DESKTOP']) / 'desktop-import.json').read_text())
request = urllib.request.Request(
    credential['origin'] + '/v1/events?after=0&limit=200',
    headers={'Authorization': 'Bearer ' + credential['deviceToken']},
)
with urllib.request.urlopen(request, timeout=10) as response:
    replay = json.load(response)
if not replay.get('events'):
    raise SystemExit('real API replay contained no persisted simulator event')

database = gateway / 'gateway.sqlcipher'
if not database.is_file() or database.stat().st_size == 0:
    raise SystemExit('client-core SQLCipher database was not persisted')
reopen = subprocess.run(
    ['target/debug/openpush-gateway-simulator', 'run', str(gateway)],
    env=os.environ.copy(),
    input='{"type":"quit"}\n',
    text=True,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    timeout=20,
)
if reopen.returncode != 0:
    raise SystemExit('persisted gateway database did not reopen cleanly')
print(f'demo verified: API events={len(replay["events"])}; core DB bytes={database.stat().st_size}')
PY

kill "$(cat "$RUN/server.pid")"
wait "$(cat "$RUN/server.pid")" || true
openpush_test_compose stop
```

`prepare-vault` creates the public profile/encrypted-header bootstrap plus a developer-only SQLCipher key. Pairing creates distinct gateway and desktop credentials and signing keys. On the first run, the gateway imports a historical snapshot before enabling controls, uploads, or permits. A durable private marker prevents reuse of the same credentials if `gateway.sqlcipher` is later lost; pair a fresh gateway identity instead. The simulator accepts JSON-line controls named `incoming`, `pause`, `resume`, `outcome`, `crash-after-effect`, and `quit`; it never invokes a real carrier API.

The tracked [mobile bindings README](crates/mobile-bindings/README.md) describes
the native-only database/key inputs, manual unlock and epoch activation, bounded
outbox and snapshot draining, and disposal semantics. Android limits and native
build commands are in [apps/android/README.md](apps/android/README.md); iOS
capability gates are in [apps/ios/README.md](apps/ios/README.md).

## Checks

Run the smallest applicable check before relying on a change:

```sh
just cargo-fmt
just lint
just server-test
just contracts-check
just desktop-test
just android-test
just ios-test
just audit-dependencies
just audit-secrets
```

`just audit-dependencies` runs `cargo deny check licenses bans sources`. `just audit-secrets` deliberately scans current source with `gitleaks --no-git`, including untracked source; it excludes ignored local credentials such as `.env` rather than treating an empty Git history as a clean scan. `just ffi-smoke` generates Kotlin and Swift UniFFI bindings, tests the Rust bindings crate, and runs a macOS Swift smoke check; it is not an iOS simulator or device check.

Android needs JDK 17, Android command-line tools, `platform-tools`,
`platforms;android-35`, `build-tools;35.0.0`, and an NDK. Run
`infra/compose/verify-android-native.sh` with `ANDROID_NDK_HOME` set, then the
Android Gradle checks documented in `apps/android/README.md`. The package is
`arm64-v8a` only. It captures new SMS broadcasts without `READ_SMS` history
access, routes only through the default SMS SIM, and does not request the
default-SMS role or `WRITE_SMS`; MMS and RCS remain unsupported. Emulator and
unit checks do not establish physical carrier behavior.

The iOS client is capability-gated and has no carrier executor in this build.
Its native build and carrier eligibility require platform-specific verification.
Desktop native connection, WebSocket reconnect, attachment/public-copy
interaction, and transparent-window click-through still need platform-specific
verification; a native click-through claim requires real input-region, focus,
and IME testing, not CSS or view hit testing alone.

## Backup and restore

Create a backup in an existing destination directory:

```sh
just backup /secure/backups
```

Allow in-flight API requests to drain first. The backup uses PostgreSQL `pg_dump`, stops SeaweedFS before copying its volume and filer metadata, and writes a versioned checksum manifest. It does not delete or prune data. On exit, it restarts **only services that were running when the backup began**; it leaves initially stopped services stopped.

Restore only into a distinct Compose project with no existing target volumes:

```sh
just restore /secure/backups/openpush-openpush-... restore-drill
```

Restore verifies the manifest and checksums, checks image/layout compatibility, and refuses existing target volumes. It starts PostgreSQL only to load the dump. It does not start migrations, the API, or carrier processing, and it does not automatically execute anything after restore. Reconcile carrier attempts, encrypted attachment consistency, and duplicate or outcome-unknown command ledgers before deliberately starting services. Do not use `docker compose down -v` as a recovery shortcut.

**A server restore also rewinds credential/public-link revocations, key-profile changes and transport cursors.** Keep the restored API private and carrier processing stopped while reconciling:

- Reapply device and public-link revocations made after the backup, or replace affected device credentials, before reopening access. A previously revoked token or public image link can become valid again in an older dump. Establish a trusted owner credential before administering the restored vault, and reconcile post-backup key-profile activations from trusted recovery material.
- Explicitly resynchronize **every existing client**, or enroll it with a fresh device identity. Do not rely on automatic `resync_required`: once the server reuses a cursor range, a client whose old cursor falls inside that range can silently miss records. This foundation has no server-restore generation marker.
- Keep old client stores while reconciling drafts, unuploaded records and carrier attempts. A gateway restored from a client backup must use the restore guard and a fresh enrollment before carrier execution; never clear an uncertain attempt merely to resume sending. Recovery of changes newer than the server backup is a separate reconciliation task.

The automated encrypted recovery drill proves restoration into a fresh namespace, client decryption and historical-command blocking. It does not prove transparent continuation by already-connected clients or automatic restoration of post-backup revocations.

This is a single-node foundation, not HA. Budget storage for a complete backup and restore. Replay retention defaults to 30 days and may be set from 1 to 3650 days with `OPENPUSH_REPLAY_RETENTION_DAYS`; an expired or ahead cursor requires snapshot resynchronization. Immutable snapshots are retained separately. Do not remove historical key material merely because transport replay rows have aged out.

## Release boundary

Releases are git tags; checked-in manifests keep development placeholder versions and CI stamps the computed version into each build.

- **Prereleases:** when CI passes for a push to `main`, it publishes the GitHub prerelease `vX.Y.Z-main.N`, where `X.Y.Z` is the next version predicted from Conventional Commits and `N` counts commits since the last stable tag. Only the latest green run publishes; queued runs superseded by a newer push are skipped. The newest 10 prereleases are kept. The server image is pushed to `ghcr.io/mattv8/openpush-server` as `X.Y.Z-main.N` and `edge`.
- **Stable releases:** run the **Release** workflow from `main`. Inputs: `bump` (`auto`, `patch`, `minor`, `major`), an optional explicit `version` (`X.Y.Z`, for example `1.0.0`), and `dry_run` (build without publishing). The workflow requires a successful CI push run for the current `main` commit, rebuilds with the stable version, and creates the `vX.Y.Z` tag only when it publishes the release. Images are tagged `X.Y.Z`, `X.Y`, `latest`, and `X` from `1.0.0`.
- **Assets:** Android debug APK and unsigned release APK/AAB; Linux AppImage, deb, and rpm (stable only); macOS DMG and app archive; Windows MSI and NSIS installer; `SHA256SUMS`; generated release notes. iOS artifacts stay in CI runs and are not published.
- On Windows, uninstall a prerelease MSI before installing the stable MSI of the same `X.Y.Z`.

Releases do not sign, notarize, publish to stores, or turn unsigned artifacts into signed releases. Store readiness additionally requires the appropriate Apple, Windows, Android, update-signing, privacy, policy, recovery, and review work. Keep signing credentials only in protected CI environment secrets; never generate or commit them in this repository.

Preview the next version with `just version --channel prerelease`. Run `just release-test` after changing `infra/release/` or the release workflows; it needs git-cliff 2.14.2 on `PATH`, which `infra/release/install-git-cliff.sh <dir>` installs.
