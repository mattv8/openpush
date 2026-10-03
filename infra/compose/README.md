# Peppy backup and restore

Use these procedures from the repository root. Read this document before a recovery operation. A restore can revive revoked credentials and public links, reuse cursor ranges, and leave carrier outcomes uncertain.

## Backup

Stop the persistent development container before an operator backup so it cannot write through its private-network credentials while the backup pauses storage:

```sh
just dev-down
```

Create a backup in an existing destination directory:

```sh
just backup /secure/backups
```

Allow in-flight API requests to drain before the command. The script uses PostgreSQL `pg_dump`, stops SeaweedFS before copying its volume and filer metadata, and writes a versioned checksum manifest. It does not delete or prune data. On exit, it restarts only API and SeaweedFS services that ran when backup began; it leaves initially stopped services stopped.

## Restore

Restore only to a distinct Compose project with no existing target volumes:

```sh
just restore /secure/backups/peppy-peppy-... restore-drill
```

The restore verifies the manifest and checksums, verifies image/layout compatibility, and refuses existing target volumes. It starts PostgreSQL only to load the dump. It leaves migrations, the API, and carrier processing stopped, and it performs no automatic post-restore execution. Do not use `docker compose down -v` as a recovery shortcut.

Before you reopen access or carrier processing:

- Reapply device and public-link revocations that occurred after the backup, or replace affected device credentials. An old dump can make a revoked token or public image link valid again.
- Reconcile post-backup key-profile activations from trusted recovery material and establish a trusted owner credential.
- Resynchronize every existing client or enroll it with a fresh device identity. Peppy has no restore-generation marker, so a reused cursor range can make an old client miss records without an automatic resync signal.
- Keep old client stores while you reconcile drafts, unuploaded records, encrypted attachments, and carrier attempts. A gateway restored from a client backup needs the restore guard and a fresh enrollment before carrier execution. Do not clear an uncertain attempt to resume sending.

Snapshot history does not execute carrier work. The automated encrypted recovery drill proves a fresh-namespace restore, client decryption, and historical-command blocking. It does not prove continuous existing-client operation or automatic recovery of post-backup revocations. Carrier outcomes remain uncertain until an operator reconciles them.

## Key material, capacity, and retention

Keep historical passphrases and key-profile material for history that must remain readable. Epoch rotation is manual; there is no automatic recovery when required historical material is lost.

Peppy is a single-node foundation, not HA. Reserve storage for complete backup and restore data. `PEPPY_REPLAY_RETENTION_DAYS` defaults to `30` and accepts `1` through `3650`; expired or ahead cursors require snapshot resynchronization. Immutable snapshots have separate retention. Do not discard historical key material because transport replay rows aged out.
