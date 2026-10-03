//! Contact photos as encrypted attachments inside contact events.
//!
//! * Local contact bodies, ledger rows and every view carry only `photo: {attachment_id}`.
//!   The full `MediaDescriptor` (with its file key) is injected at seal time from the
//!   SQLCipher `attachments` row and exists only inside encrypted payloads. A row whose
//!   photo has no remote object ID yet stays unsealed; nothing else is held.
//! * Receivers validate the descriptor, record `pending_download` metadata through the
//!   generic media path, and strip the descriptor before the contact projection sees it.
//! * `contact_photo_references` is the fourth reference class for local GC: live contacts
//!   (no expiry), tombstones (restore window) and edit requests (until expiry).
//! * Each sealed photo-bearing envelope gets durable registration rows. The envelope is
//!   withheld from `pending_outbox` until native acknowledges `POST
//!   /v1/attachments/{remote_id}/references` for every row.
//! * Photos prepared on this device (`contact_photo_attachments`) that lose every active
//!   reference become reclaim candidates. Only the server proves release (`DELETE` 204/404);
//!   409 keeps the candidate and reschedules it (`next_due_at`, exponential, capped daily,
//!   never abandoned). Received or untracked photos are never reclaimed.
//! * A photo whose remote object was released is never referenced again: a new local use is
//!   re-encrypted from verified local bytes into a fresh attachment (`contact_photo_aliases`
//!   maps old -> fresh), or fails with `contact photo unavailable`.
//! * A held row whose photo can no longer resolve is never rewritten: it stays held and is
//!   reported in `unavailable` (`contact_photo_unavailable`).
use crate::media::{self, AttachmentState, MediaDescriptor, STREAM_VERSION};
use crate::{
    AttachmentId, Error, PrivatePayload, VaultId, attachment_row, store_remote_media, to_i64,
};
use peppy_crypto::FileKey;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// Tombstone restore window; matches the server's tombstone purge horizon.
const RESTORE_RETENTION_SECONDS: i64 = 90 * 24 * 60 * 60;
const PHOTO_MEDIA_TYPE: &str = "image/jpeg";
const MAX_PHOTO_BYTES: u64 = 64 * 1024;
/// Reclaim retry after a 409: 1h, 2h, 4h, ... capped at one day; never abandoned.
const RECLAIM_BASE_DELAY_SECONDS: i64 = 60 * 60;
const RECLAIM_MAX_DELAY_SECONDS: i64 = 24 * 60 * 60;

/// Whether an unsealed outbox row references `col` directly or through an alias.
macro_rules! held_by_outbox {
    ($col:literal) => {
        concat!(
            "EXISTS(SELECT 1 FROM outbox o WHERE o.state='unsealed' AND (instr(CAST(o.plain AS TEXT), ",
            $col,
            ")>0 OR EXISTS(SELECT 1 FROM contact_photo_aliases al WHERE al.new_id=",
            $col,
            " AND instr(CAST(o.plain AS TEXT), al.old_id)>0)))"
        )
    };
}

/// Tracked contact photos still awaiting upload: prepared here and needed by an active
/// reference or by a held (unsealed) outbox row. Shared with `Client::pending_uploads`.
pub(crate) const UPLOAD_IDS_SQL: &str = concat!(
    "SELECT a.attachment_id FROM attachments a JOIN contact_photo_attachments t ON t.attachment_id=a.attachment_id WHERE a.state='pending_upload' AND (EXISTS(SELECT 1 FROM contact_photo_references r WHERE r.attachment_id=a.attachment_id AND (r.retain_until IS NULL OR r.retain_until>CAST(strftime('%s','now') AS INTEGER))) OR ",
    held_by_outbox!("a.attachment_id"),
    ")"
);

pub(crate) fn initialize(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        "\
CREATE TABLE IF NOT EXISTS contact_photo_attachments(attachment_id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS contact_photo_references(holder_kind TEXT NOT NULL, book_id TEXT NOT NULL, holder_id TEXT NOT NULL, attachment_id TEXT NOT NULL, retain_until INTEGER, PRIMARY KEY(holder_kind,book_id,holder_id));
CREATE INDEX IF NOT EXISTS contact_photo_references_attachment ON contact_photo_references(attachment_id);
CREATE TABLE IF NOT EXISTS contact_photo_registrations(envelope_id TEXT NOT NULL, attachment_id TEXT NOT NULL, remote_object_id TEXT NOT NULL, producer_sequence INTEGER NOT NULL, acknowledged INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(envelope_id,attachment_id));
CREATE TABLE IF NOT EXISTS contact_photo_reclaims(attachment_id TEXT PRIMARY KEY, remote_object_id TEXT NOT NULL, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, next_due_at INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS contact_photo_aliases(old_id TEXT PRIMARY KEY, new_id TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS contact_photo_unavailable(outbox_sequence INTEGER PRIMARY KEY, attachment_id TEXT, reason TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS contact_photo_received(attachment_id TEXT PRIMARY KEY, rejected INTEGER NOT NULL DEFAULT 0);
",
    )?;
    // Pre-release databases: add the retry schedule and resume any abandoned candidate.
    let has_due: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('contact_photo_reclaims') WHERE name='next_due_at')",
        [],
        |r| r.get(0),
    )?;
    if !has_due {
        conn.execute_batch(
            "ALTER TABLE contact_photo_reclaims ADD COLUMN next_due_at INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    conn.execute(
        "UPDATE contact_photo_reclaims SET state='pending' WHERE state='exhausted'",
        [],
    )?;
    Ok(())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn invalid() -> Error {
    Error::InvalidRequest("invalid contact photo")
}

/// Marks an attachment produced by `prepare_contact_photo` as a tracked contact photo.
pub(crate) fn track_prepared(conn: &Connection, id: AttachmentId) -> Result<(), Error> {
    conn.execute(
        "INSERT OR IGNORE INTO contact_photo_attachments(attachment_id) VALUES(?)",
        [id.to_string()],
    )?;
    Ok(())
}

/// Reference ID from `"<uuid>"` or `{"attachment_id":"<uuid>", ...}`.
fn reference_id(photo: &Value) -> Option<AttachmentId> {
    let text = match photo {
        Value::String(s) => s.as_str(),
        Value::Object(o) => o.get("attachment_id")?.as_str()?,
        _ => return None,
    };
    text.parse().ok()
}

fn photo_row_ok(media_type: &str, plaintext_bytes: u64) -> bool {
    media_type == PHOTO_MEDIA_TYPE && plaintext_bytes <= MAX_PHOTO_BYTES
}

fn unavailable() -> Error {
    Error::InvalidRequest("contact photo unavailable")
}

/// Follows a re-preparation alias (aliases are kept flat: one hop).
fn effective_id(conn: &Connection, id: AttachmentId) -> Result<AttachmentId, Error> {
    let new: Option<String> = conn
        .query_row(
            "SELECT new_id FROM contact_photo_aliases WHERE old_id=?",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(new.and_then(|new| new.parse().ok()).unwrap_or(id))
}

/// The server confirmed release of this attachment's remote object.
fn reclaimed(conn: &Connection, id: AttachmentId) -> Result<bool, Error> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM contact_photo_reclaims WHERE attachment_id=? AND state='completed')",
        [id.to_string()],
        |r| r.get(0),
    )?)
}

fn media_root(conn: &Connection) -> Result<std::path::PathBuf, Error> {
    // Same derivation as `media::prepare_media_root`: `<database file>.media`.
    match conn.path() {
        Some(path) if !path.is_empty() => Ok(format!("{path}.media").into()),
        _ => Err(Error::Storage),
    }
}

/// Re-encrypts a released photo's verified local bytes into a fresh tracked attachment and
/// aliases the old ID to it. Missing or unverifiable local bytes are `contact photo unavailable`.
fn reprepare(
    conn: &Connection,
    vault_id: VaultId,
    old: AttachmentId,
) -> Result<AttachmentId, Error> {
    let row = attachment_row(conn, old)?.ok_or_else(unavailable)?;
    let root = media_root(conn)?;
    let bytes = media::decrypt_to_bytes(
        &root,
        old,
        &row.file_key()?,
        &media::media_aad(vault_id, old),
        row.plaintext_bytes,
    )
    .map_err(|_| unavailable())?;
    let fresh = AttachmentId::new();
    let key = FileKey::generate().map_err(|_| Error::Crypto)?;
    let encrypted = media::encrypt_bytes_into_store(
        &root,
        fresh,
        &bytes,
        &key,
        &media::media_aad(vault_id, fresh),
    )?;
    let key_bytes = zeroize::Zeroizing::new(key.with_encrypted_reference_bytes(|bytes| *bytes));
    let inserted = conn
        .execute(
            "INSERT INTO attachments(attachment_id,state,media_type,display_name,plaintext_bytes,ciphertext_bytes,ciphertext_sha256,stream_version,file_key,remote_object_id) VALUES(?,?,?,?,?,?,?,?,?,NULL)",
            params![
                fresh.to_string(),
                AttachmentState::PendingUpload.code(),
                row.media_type,
                row.display_name,
                to_i64(encrypted.plaintext_bytes)?,
                to_i64(encrypted.ciphertext_bytes)?,
                encrypted.ciphertext_sha256,
                STREAM_VERSION,
                key_bytes.as_slice()
            ],
        )
        .map_err(Error::from)
        .and_then(|_| track_prepared(conn, fresh))
        .and_then(|_| {
            conn.execute(
                "UPDATE contact_photo_aliases SET new_id=?1 WHERE new_id=?2",
                params![fresh.to_string(), old.to_string()],
            )?;
            conn.execute(
                "INSERT INTO contact_photo_aliases(old_id,new_id) VALUES(?1,?2) ON CONFLICT(old_id) DO UPDATE SET new_id=excluded.new_id",
                params![old.to_string(), fresh.to_string()],
            )?;
            Ok(())
        });
    if let Err(error) = inserted {
        let _ = std::fs::remove_file(media::cipher_path(&root, fresh));
        return Err(error);
    }
    Ok(fresh)
}

/// A local attachment usable as a contact photo (normalized JPEG of any local/remote state).
/// A photo whose remote object was already released is re-prepared, never reused.
fn require_local_photo(
    conn: &Connection,
    vault_id: VaultId,
    photo: &Value,
) -> Result<AttachmentId, Error> {
    let id = effective_id(conn, reference_id(photo).ok_or_else(invalid)?)?;
    let row = attachment_row(conn, id)?.ok_or_else(unavailable)?;
    if !photo_row_ok(&row.media_type, row.plaintext_bytes) {
        return Err(invalid());
    }
    // Only normalized photos: prepared here, or received as a contact photo and not rejected.
    // Generic attachments (e.g. MMS media) can never become contact photos.
    let kind: (bool, Option<bool>) = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM contact_photo_attachments WHERE attachment_id=?1),(SELECT rejected FROM contact_photo_received WHERE attachment_id=?1)",
        [id.to_string()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    match kind {
        (true, _) | (false, Some(false)) => {}
        (false, Some(true)) => return Err(unavailable()),
        (false, None) => return Err(invalid()),
    }
    if reclaimed(conn, id)? {
        return reprepare(conn, vault_id, id);
    }
    Ok(id)
}

/// Set-photo media that is not yet locally verified, so an OS apply permit must wait.
pub(crate) fn photo_waiting(conn: &Connection, request: &Value) -> Result<Option<String>, Error> {
    for photo in request_photos(request) {
        let Some(id) = reference_id(photo) else {
            continue;
        };
        let id = effective_id(conn, id)?;
        let ready = attachment_row(conn, id)?.is_some_and(|row| row.state.is_local());
        if !ready {
            return Ok(Some(id.to_string()));
        }
    }
    Ok(None)
}

fn present_photo(value: Option<&Value>) -> bool {
    value.is_some_and(|v| !v.is_null())
}

// ---------------------------------------------------------------------------------------
// Local enqueue / apply bookkeeping
// ---------------------------------------------------------------------------------------

/// Validates locally produced photo references and updates reference holders. Runs inside
/// the producing transaction, after the contact row was written.
pub(crate) fn on_enqueue(
    conn: &Connection,
    vault_id: VaultId,
    payload: &PrivatePayload,
) -> Result<(), Error> {
    match payload {
        PrivatePayload::ContactUpserted { book, contact } => {
            if present_photo(contact.get("photo")) {
                require_local_photo(conn, vault_id, &contact["photo"])?;
            }
            sync_contact_from_payload(conn, book.get("id"), contact)
        }
        PrivatePayload::ContactRemoved {
            book_id,
            contact_id,
            ..
        } => sync_contact(conn, book_id, contact_id),
        PrivatePayload::ContactEditRequest { request } => {
            for photo in request_photos(request) {
                require_local_photo(conn, vault_id, photo)?;
            }
            hold_request(conn, request)
        }
        _ => Ok(()),
    }
}

/// Reference bookkeeping after a peer (or own echoed) contact event was applied.
pub(crate) fn after_apply(conn: &Connection, payload: &PrivatePayload) -> Result<(), Error> {
    match payload {
        PrivatePayload::ContactUpserted { book, contact } => {
            sync_contact_from_payload(conn, book.get("id"), contact)
        }
        PrivatePayload::ContactRemoved {
            book_id,
            contact_id,
            ..
        } => sync_contact(conn, book_id, contact_id),
        PrivatePayload::ContactEditRequest { request } => hold_request(conn, request),
        _ => Ok(()),
    }
}

fn sync_contact_from_payload(
    conn: &Connection,
    book_id: Option<&Value>,
    contact: &Value,
) -> Result<(), Error> {
    let book_id = book_id
        .and_then(Value::as_str)
        .or_else(|| contact.get("book_id").and_then(Value::as_str));
    match (book_id, contact.get("id").and_then(Value::as_str)) {
        (Some(book_id), Some(contact_id)) => sync_contact(conn, book_id, contact_id),
        _ => Ok(()),
    }
}

/// Derives the contact holder from the stored (revision-gated) row, so stale events can never
/// move a reference.
pub(crate) fn sync_contact(
    conn: &Connection,
    book_id: &str,
    contact_id: &str,
) -> Result<(), Error> {
    let row: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT body,deleted_at FROM contacts WHERE book_id=? AND id=?",
            params![book_id, contact_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let held = row.and_then(|(body, deleted_at)| {
        let body: Value = serde_json::from_str(&body).ok()?;
        let id = reference_id(body.get("photo")?)?;
        Some((
            id,
            deleted_at.map(|at| at.saturating_add(RESTORE_RETENTION_SECONDS)),
        ))
    });
    let held = held
        .map(|(id, retain)| effective_id(conn, id).map(|id| (id, retain)))
        .transpose()?;
    set_holder(conn, "contact", book_id, contact_id, held)
}

fn request_photos(request: &Value) -> Vec<&Value> {
    let mut photos = Vec::new();
    if let Some(op) = request.get("photo_op")
        && op.get("op").and_then(Value::as_str) == Some("set")
        && let Some(reference) = op.get("attachment_id")
    {
        photos.push(reference);
    }
    if present_photo(request.get("photo")) {
        photos.push(&request["photo"]);
    }
    photos
}

fn hold_request(conn: &Connection, request: &Value) -> Result<(), Error> {
    let (Some(book_id), Some(request_id)) = (
        request.get("book_id").and_then(Value::as_str),
        request.get("request_id").and_then(Value::as_str),
    ) else {
        return Ok(());
    };
    let expires = request.get("expires_at").and_then(Value::as_i64);
    let held = request_photos(request)
        .first()
        .and_then(|photo| reference_id(photo))
        .map(|id| effective_id(conn, id).map(|id| (id, Some(expires.unwrap_or_else(now)))))
        .transpose()?;
    set_holder(conn, "edit", book_id, request_id, held)
}

fn set_holder(
    conn: &Connection,
    kind: &str,
    book_id: &str,
    holder_id: &str,
    held: Option<(AttachmentId, Option<i64>)>,
) -> Result<(), Error> {
    match held {
        Some((id, retain_until)) => {
            conn.execute(
                "INSERT INTO contact_photo_references(holder_kind,book_id,holder_id,attachment_id,retain_until) VALUES(?,?,?,?,?) ON CONFLICT(holder_kind,book_id,holder_id) DO UPDATE SET attachment_id=excluded.attachment_id,retain_until=excluded.retain_until",
                params![kind, book_id, holder_id, id.to_string(), retain_until],
            )?;
            // A photo referenced again (e.g. restored contact) is no longer a reclaim candidate.
            conn.execute(
                "DELETE FROM contact_photo_reclaims WHERE attachment_id=? AND state!='completed'",
                [id.to_string()],
            )?;
        }
        None => {
            conn.execute(
                "DELETE FROM contact_photo_references WHERE holder_kind=? AND book_id=? AND holder_id=?",
                params![kind, book_id, holder_id],
            )?;
        }
    }
    // Released photos become reclaim candidates lazily in `transfer_state`; nothing is deleted here.
    Ok(())
}

/// Fourth reference class for `discard_unreferenced_attachment`: an active holder, a held or
/// unregistered outbox row, or an unfinished server reclaim of a tracked uploaded photo.
pub(crate) fn blocks_discard(conn: &Connection, id: AttachmentId) -> Result<bool, Error> {
    let id = id.to_string();
    Ok(conn.query_row(
        concat!(
            "SELECT EXISTS(SELECT 1 FROM contact_photo_references WHERE attachment_id=?1 AND (retain_until IS NULL OR retain_until>?2))
            OR EXISTS(SELECT 1 FROM contact_photo_registrations WHERE attachment_id=?1 AND acknowledged=0)
            OR ",
            held_by_outbox!("?1"),
            "
            OR EXISTS(SELECT 1 FROM contact_photo_attachments t JOIN attachments a ON a.attachment_id=t.attachment_id
                      WHERE t.attachment_id=?1 AND a.remote_object_id IS NOT NULL
                      AND NOT EXISTS(SELECT 1 FROM contact_photo_reclaims c WHERE c.attachment_id=?1 AND c.state='completed'))"
        ),
        params![id, now()],
        |r| r.get(0),
    )?)
}

// ---------------------------------------------------------------------------------------
// Seal / ingest rewriting
// ---------------------------------------------------------------------------------------

enum Wire {
    Ready(AttachmentId, String, Value),
    /// Waiting for upload (no remote object ID yet).
    Hold,
    /// Can never resolve as referenced; the row is held and reported, never rewritten.
    Unavailable(Option<AttachmentId>, &'static str),
}

fn wire(conn: &Connection, reference: &Value) -> Result<Wire, Error> {
    let Some(id) = reference_id(reference) else {
        return Ok(Wire::Unavailable(None, "invalid_reference"));
    };
    let id = effective_id(conn, id)?;
    let Some(row) = attachment_row(conn, id)? else {
        return Ok(Wire::Unavailable(Some(id), "missing"));
    };
    if !photo_row_ok(&row.media_type, row.plaintext_bytes) {
        return Ok(Wire::Unavailable(Some(id), "not_contact_photo"));
    }
    if reclaimed(conn, id)? {
        // Never hand out a released remote object.
        return Ok(Wire::Unavailable(Some(id), "reclaimed"));
    }
    let Some(remote_object_id) = row.remote_object_id.clone() else {
        return Ok(Wire::Hold);
    };
    let descriptor = MediaDescriptor {
        attachment_id: id,
        remote_object_id: remote_object_id.clone(),
        media_type: row.media_type.clone(),
        display_name: row.display_name.clone(),
        plaintext_bytes: row.plaintext_bytes,
        ciphertext_bytes: row.ciphertext_bytes,
        ciphertext_sha256: row.ciphertext_sha256.clone(),
        stream_version: STREAM_VERSION,
        file_key: *row.file_key,
    };
    let descriptor = serde_json::to_value(&descriptor).map_err(|_| Error::Database)?;
    Ok(Wire::Ready(id, remote_object_id, descriptor))
}

/// (local attachment ID, remote object ID) pairs an envelope must register before upload.
pub(crate) type Registrations = Vec<(AttachmentId, String)>;

/// Resolves one reference slot. `Some(wire)` (Hold/Unavailable) stops sealing this row.
fn seal_slot(
    conn: &Connection,
    slot: &mut Value,
    with_descriptor: impl FnOnce(&mut Value, AttachmentId, Value),
    refs: &mut Registrations,
) -> Result<Option<Wire>, Error> {
    match wire(conn, slot)? {
        Wire::Ready(id, remote, descriptor) => {
            with_descriptor(slot, id, descriptor);
            if !refs.iter().any(|(existing, _)| *existing == id) {
                refs.push((id, remote));
            }
            Ok(None)
        }
        other => Ok(Some(other)),
    }
}

fn photo_slot(slot: &mut Value, id: AttachmentId, descriptor: Value) {
    *slot = json!({"attachment_id": id.to_string(), "descriptor": descriptor});
}

fn seal_photo(
    conn: &Connection,
    parent: &mut Value,
    key: &str,
    refs: &mut Registrations,
) -> Result<Option<Wire>, Error> {
    if !present_photo(parent.get(key)) {
        return Ok(None);
    }
    seal_slot(conn, &mut parent[key], photo_slot, refs)
}

fn resolve_payload(
    conn: &Connection,
    payload: PrivatePayload,
    refs: &mut Registrations,
) -> Result<Result<PrivatePayload, Wire>, Error> {
    Ok(Ok(match payload {
        PrivatePayload::ContactUpserted { book, mut contact } => {
            if let Some(stop) = seal_photo(conn, &mut contact, "photo", refs)? {
                return Ok(Err(stop));
            }
            PrivatePayload::ContactUpserted { book, contact }
        }
        PrivatePayload::ContactRemoved {
            book_id,
            contact_id,
            mut tombstone,
        } => {
            if let Some(restored) = tombstone.get_mut("restored_contact")
                && let Some(stop) = seal_photo(conn, restored, "photo", refs)?
            {
                return Ok(Err(stop));
            }
            PrivatePayload::ContactRemoved {
                book_id,
                contact_id,
                tombstone,
            }
        }
        PrivatePayload::ContactEditRequest { mut request } => {
            if let Some(stop) = seal_photo(conn, &mut request, "photo", refs)? {
                return Ok(Err(stop));
            }
            if request.pointer("/photo_op/op").and_then(Value::as_str) == Some("set") {
                let op = &mut request["photo_op"];
                let mut reference = op.get("attachment_id").cloned().unwrap_or(Value::Null);
                let mut sealed = None;
                if let Some(stop) = seal_slot(
                    conn,
                    &mut reference,
                    |_, id, descriptor| sealed = Some((id, descriptor)),
                    refs,
                )? {
                    return Ok(Err(stop));
                }
                if let Some((id, descriptor)) = sealed {
                    op["attachment_id"] = json!(id.to_string());
                    op["descriptor"] = descriptor;
                }
            }
            PrivatePayload::ContactEditRequest { request }
        }
        other => other,
    }))
}

/// Seal-time descriptor injection for outbox row `sequence`. `None` holds the row: while its
/// photo awaits upload, or indefinitely (reported via `unavailable`) when the reference can no
/// longer resolve. The user's photo/removal intent is never rewritten. Compaction metadata
/// captured at enqueue is untouched.
pub(crate) fn resolve_for_seal(
    conn: &Connection,
    sequence: i64,
    payload: PrivatePayload,
) -> Result<Option<(PrivatePayload, Registrations)>, Error> {
    let mut refs = Vec::new();
    let outcome = resolve_payload(conn, payload, &mut refs)?;
    if let Err(Wire::Unavailable(id, reason)) = &outcome {
        conn.execute(
            "INSERT INTO contact_photo_unavailable(outbox_sequence,attachment_id,reason) VALUES(?,?,?) ON CONFLICT(outbox_sequence) DO UPDATE SET attachment_id=excluded.attachment_id,reason=excluded.reason",
            params![sequence, id.map(|id| id.to_string()), reason],
        )?;
        return Ok(None);
    }
    conn.execute(
        "DELETE FROM contact_photo_unavailable WHERE outbox_sequence=?",
        [sequence],
    )?;
    Ok(outcome.ok().map(|payload| (payload, refs)))
}

/// Records durable registration work for a just-sealed envelope.
pub(crate) fn record_registrations(
    conn: &Connection,
    envelope_id: &str,
    sequence: i64,
    refs: &Registrations,
) -> Result<(), Error> {
    for (id, remote) in refs {
        conn.execute(
            "INSERT OR IGNORE INTO contact_photo_registrations(envelope_id,attachment_id,remote_object_id,producer_sequence,acknowledged) VALUES(?,?,?,?,0)",
            params![envelope_id, id.to_string(), remote, sequence],
        )?;
    }
    Ok(())
}

/// Validates a wire photo descriptor, records `pending_download` metadata, and returns the
/// local reference form. `None` rejects the payload.
fn ingest_photo(conn: &Connection, photo: &Value) -> Result<Option<Value>, Error> {
    let Some(obj) = photo.as_object() else {
        return Ok(None);
    };
    if obj.len() != 2 {
        return Ok(None);
    }
    let (Some(id), Some(descriptor)) = (reference_id(photo), obj.get("descriptor")) else {
        return Ok(None);
    };
    Ok(ingest_descriptor(conn, id, descriptor)?.map(|id| json!({"attachment_id": id.to_string()})))
}

fn ingest_descriptor(
    conn: &Connection,
    id: AttachmentId,
    descriptor: &Value,
) -> Result<Option<AttachmentId>, Error> {
    let Ok(descriptor) = serde_json::from_value::<MediaDescriptor>(descriptor.clone()) else {
        return Ok(None);
    };
    if descriptor.attachment_id != id
        || !descriptor.is_valid()
        || !photo_row_ok(&descriptor.media_type, descriptor.plaintext_bytes)
        || !store_remote_media(conn, std::slice::from_ref(&descriptor))?
    {
        return Ok(None);
    }
    // Received as a contact photo: its decrypted bytes are verified at install.
    conn.execute(
        "INSERT OR IGNORE INTO contact_photo_received(attachment_id) VALUES(?)",
        [id.to_string()],
    )?;
    Ok(Some(id))
}

/// A received contact photo whose decrypted bytes must pass `validate_normalized` before it
/// becomes available.
pub(crate) fn is_received_photo(conn: &Connection, id: AttachmentId) -> Result<bool, Error> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM contact_photo_received WHERE attachment_id=?)",
        [id.to_string()],
        |r| r.get(0),
    )?)
}

/// Marks a received photo whose decrypted bytes failed validation. It stays not available,
/// leaves `pending_downloads`, and cannot be referenced again.
pub(crate) fn reject_received(conn: &Connection, id: AttachmentId) -> Result<(), Error> {
    conn.execute(
        "UPDATE contact_photo_received SET rejected=1 WHERE attachment_id=?",
        [id.to_string()],
    )?;
    Ok(())
}

/// Excluded from `Client::pending_downloads`.
pub(crate) const REJECTED_DOWNLOADS_SQL: &str =
    "SELECT attachment_id FROM contact_photo_received WHERE rejected=1";

fn ingest_slot(conn: &Connection, parent: &mut Value, key: &str) -> Result<bool, Error> {
    if !present_photo(parent.get(key)) {
        return Ok(true);
    }
    match ingest_photo(conn, &parent[key])? {
        Some(local) => {
            parent[key] = local;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Receive-side counterpart of `resolve_for_seal`. `None` quarantines the record.
pub(crate) fn ingest(
    conn: &Connection,
    payload: PrivatePayload,
) -> Result<Option<PrivatePayload>, Error> {
    Ok(Some(match payload {
        PrivatePayload::ContactUpserted { book, mut contact } => {
            if !ingest_slot(conn, &mut contact, "photo")? {
                return Ok(None);
            }
            PrivatePayload::ContactUpserted { book, contact }
        }
        PrivatePayload::ContactRemoved {
            book_id,
            contact_id,
            mut tombstone,
        } => {
            if let Some(restored) = tombstone.get_mut("restored_contact")
                && !ingest_slot(conn, restored, "photo")?
            {
                return Ok(None);
            }
            PrivatePayload::ContactRemoved {
                book_id,
                contact_id,
                tombstone,
            }
        }
        PrivatePayload::ContactEditRequest { mut request } => {
            if !ingest_slot(conn, &mut request, "photo")? {
                return Ok(None);
            }
            if request.pointer("/photo_op/op").and_then(Value::as_str) == Some("set") {
                let op = &mut request["photo_op"];
                let (Some(id), Some(descriptor)) = (
                    op.get("attachment_id").and_then(reference_id),
                    op.get("descriptor").cloned(),
                ) else {
                    return Ok(None);
                };
                if ingest_descriptor(conn, id, &descriptor)?.is_none() {
                    return Ok(None);
                }
                op["attachment_id"] = json!(id.to_string());
                if let Some(op) = op.as_object_mut() {
                    op.remove("descriptor");
                }
            }
            PrivatePayload::ContactEditRequest { request }
        }
        other => other,
    }))
}

/// View form: `photo` becomes `{attachment_id, available}`; anything else is removed.
pub(crate) fn present(conn: &Connection, mut contact: Value) -> Value {
    let Some(obj) = contact.as_object_mut() else {
        return contact;
    };
    if let Some(photo) = obj.remove("photo")
        && let Some(id) = reference_id(&photo)
    {
        let id = effective_id(conn, id).unwrap_or(id);
        let available = attachment_row(conn, id)
            .ok()
            .flatten()
            .is_some_and(|row| row.state.is_local());
        obj.insert(
            "photo".into(),
            json!({"attachment_id": id.to_string(), "available": available}),
        );
    }
    contact
}

// ---------------------------------------------------------------------------------------
// Native work queue
// ---------------------------------------------------------------------------------------

fn materialize_reclaims(conn: &Connection, now: i64) -> Result<(), Error> {
    conn.execute(
        concat!(
            "INSERT OR IGNORE INTO contact_photo_reclaims(attachment_id,remote_object_id,state,attempts,next_due_at)
         SELECT a.attachment_id, a.remote_object_id, 'pending', 0, ?1
         FROM contact_photo_attachments t JOIN attachments a ON a.attachment_id=t.attachment_id
         WHERE a.remote_object_id IS NOT NULL
           AND NOT EXISTS(SELECT 1 FROM contact_photo_references r WHERE r.attachment_id=a.attachment_id AND (r.retain_until IS NULL OR r.retain_until>?1))
           AND NOT EXISTS(SELECT 1 FROM contact_photo_registrations g WHERE g.attachment_id=a.attachment_id AND g.acknowledged=0)
           AND NOT ",
            held_by_outbox!("a.attachment_id")
        ),
        [now],
    )?;
    Ok(())
}

/// Upload IDs (reserve with `reference_tracking:true`), registrations to POST before the
/// referencing envelope is published, and server reclaim candidates.
pub(crate) fn transfer_state(conn: &Connection, own_device: &str) -> Result<String, Error> {
    transfer_state_at(conn, own_device, now())
}

fn transfer_state_at(conn: &Connection, own_device: &str, now: i64) -> Result<String, Error> {
    materialize_reclaims(conn, now)?;
    let uploads = conn
        .prepare(&format!("{UPLOAD_IDS_SQL} ORDER BY a.rowid"))?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|id| json!({"attachment_id": id, "reference_tracking": true}))
        .collect::<Vec<_>>();
    let registrations = conn
        .prepare("SELECT envelope_id,attachment_id,remote_object_id,producer_sequence FROM contact_photo_registrations WHERE acknowledged=0 ORDER BY producer_sequence,attachment_id")?
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(envelope_id, local, remote, sequence)| {
            json!({
                "envelope_id": envelope_id,
                "local_attachment_id": local,
                "attachment_id": remote,
                "producer_device_id": own_device,
                "producer_sequence": sequence.to_string(),
            })
        })
        .collect::<Vec<_>>();
    let generation: Option<String> = conn
        .query_row(
            "SELECT server_compaction_generation FROM snapshot_generations WHERE server_compaction_generation IS NOT NULL ORDER BY generation DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    // Only candidates whose retry time has come; deferred ones stay durable.
    let candidates = conn
        .prepare("SELECT attachment_id,remote_object_id,attempts,next_due_at FROM contact_photo_reclaims WHERE state='pending' AND next_due_at<=? ORDER BY next_due_at,attachment_id")?
        .query_map([now], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let deferred: i64 = conn.query_row(
        "SELECT COUNT(*) FROM contact_photo_reclaims WHERE state='pending' AND next_due_at>?",
        [now],
        |r| r.get(0),
    )?;
    let unavailable = conn
        .prepare("SELECT outbox_sequence,attachment_id,reason FROM contact_photo_unavailable ORDER BY outbox_sequence")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?)))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(sequence, id, reason)| json!({"producer_sequence": sequence.to_string(), "local_attachment_id": id, "reason": reason}))
        .collect::<Vec<_>>();
    let mut reclaims = Vec::with_capacity(candidates.len());
    for (local, remote, attempts, _) in candidates {
        // Each acknowledged reference with the server cursor this device journaled it at.
        let rows = conn
            .prepare("SELECT g.producer_sequence,(SELECT MAX(j.cursor) FROM journal j WHERE j.envelope_id=g.envelope_id) FROM contact_photo_registrations g WHERE g.attachment_id=? AND g.acknowledged=1 ORDER BY g.producer_sequence")?
            .query_map([&local], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        // The release cutoff must cover every reference; unknown until all were echoed back.
        let release_after_cursor = rows
            .iter()
            .map(|(_, cursor)| *cursor)
            .collect::<Option<Vec<_>>>()
            .and_then(|cursors| cursors.into_iter().max())
            .map(|cursor| cursor.to_string());
        let references = rows
            .into_iter()
            .map(|(sequence, cursor)| {
                json!({
                    "producer_device_id": own_device,
                    "producer_sequence": sequence.to_string(),
                    "source_cursor": cursor.map(|c| c.to_string()),
                })
            })
            .collect::<Vec<_>>();
        reclaims.push(json!({
            "attachment_id": local,
            "remote_object_id": remote,
            "server_compaction_generation": generation,
            "release_after_cursor": release_after_cursor,
            "references": references,
            "attempts": attempts,
        }));
    }
    Ok(json!({
        "schema_version": 1,
        "uploads": uploads,
        "registrations": registrations,
        "reclaims": reclaims,
        "reclaims_deferred": deferred,
        "unavailable": unavailable,
    })
    .to_string())
}

fn parse_input(input: &str) -> Result<Value, Error> {
    let v: Value = serde_json::from_str(input).map_err(|_| invalid())?;
    if v.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err(invalid());
    }
    Ok(v)
}
fn uuid_field(v: &Value, key: &str) -> Result<String, Error> {
    v.get(key)
        .and_then(Value::as_str)
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(|u| u.to_string())
        .ok_or_else(invalid)
}

/// `{schema_version:1, envelope_id, attachment_id: <remote id>}` after the server accepted
/// the reference (204). Idempotent. Releases the envelope once all its rows are acknowledged.
pub(crate) fn acknowledge_reference(conn: &Connection, input: &str) -> Result<String, Error> {
    let v = parse_input(input)?;
    let envelope_id = uuid_field(&v, "envelope_id")?;
    let remote = uuid_field(&v, "attachment_id")?;
    let changed = conn.execute(
        "UPDATE contact_photo_registrations SET acknowledged=1 WHERE envelope_id=? AND remote_object_id=?",
        params![envelope_id, remote],
    )?;
    if changed == 0 {
        return Err(Error::NotFound);
    }
    let remaining: i64 = conn.query_row(
        "SELECT COUNT(*) FROM contact_photo_registrations WHERE envelope_id=? AND acknowledged=0",
        [&envelope_id],
        |r| r.get(0),
    )?;
    Ok(json!({"status":"acknowledged","envelope_id":envelope_id,"envelope_released":remaining == 0}).to_string())
}

/// `{schema_version:1, attachment_id: <local id>, http_status: 204|404|409}` from
/// `DELETE /v1/attachments/{remote_object_id}`. 204/404 complete; 409 keeps the candidate and
/// reschedules it with bounded exponential backoff (1h doubling, capped at 24h, unbounded tries).
pub(crate) fn acknowledge_reclaim(conn: &Connection, input: &str) -> Result<String, Error> {
    acknowledge_reclaim_at(conn, input, now())
}

fn reclaim_delay(attempts: i64) -> i64 {
    let shift = attempts.saturating_sub(1).clamp(0, 16) as u32;
    RECLAIM_BASE_DELAY_SECONDS
        .saturating_mul(1i64 << shift)
        .min(RECLAIM_MAX_DELAY_SECONDS)
}

fn acknowledge_reclaim_at(conn: &Connection, input: &str, now: i64) -> Result<String, Error> {
    let v = parse_input(input)?;
    let id = uuid_field(&v, "attachment_id")?;
    let status = v
        .get("http_status")
        .and_then(Value::as_u64)
        .ok_or_else(invalid)?;
    let row: Option<(String, i64, i64)> = conn
        .query_row(
            "SELECT state,attempts,next_due_at FROM contact_photo_reclaims WHERE attachment_id=?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((state, attempts, next_due_at)) = row else {
        return Err(Error::NotFound);
    };
    let (next, next_due_at) = match (state.as_str(), status) {
        ("completed", 204 | 404 | 409) => ("completed", next_due_at),
        ("pending", 204 | 404) => {
            conn.execute(
                "UPDATE contact_photo_reclaims SET state='completed' WHERE attachment_id=?",
                [&id],
            )?;
            ("completed", next_due_at)
        }
        ("pending", 409) => {
            let attempts = attempts.saturating_add(1);
            let due = now.saturating_add(reclaim_delay(attempts));
            conn.execute(
                "UPDATE contact_photo_reclaims SET attempts=?,next_due_at=? WHERE attachment_id=?",
                params![attempts, due, id],
            )?;
            ("pending", due)
        }
        _ => return Err(invalid()),
    };
    Ok(json!({"status": next, "attachment_id": id, "next_due_at": next_due_at}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, ClientConfig, DatabaseKey, DeviceId, KeyProfile};
    use image::ImageEncoder;
    use peppy_crypto::{create_vault_check_header, derive_root_key};

    const PASS: &str = "correct horse battery staple";

    fn client(dir: &tempfile::TempDir) -> (Client, ClientConfig) {
        let vault_id = VaultId::new();
        let profile = KeyProfile::new(vault_id.0, 1).unwrap();
        let root = derive_root_key(PASS, &profile).unwrap();
        let header = create_vault_check_header(&root, profile.clone()).unwrap();
        let config = ClientConfig {
            database_path: dir.path().join("m.db"),
            vault_id,
            device_id: DeviceId::new(),
        };
        let client = Client::open(config.clone(), DatabaseKey::new(&[3; 32]).unwrap()).unwrap();
        client.unlock(&profile, &header, PASS).unwrap();
        // Connected to a compaction-capable server (contacts fail closed otherwise).
        client.set_server_compaction_state(true, true).unwrap();
        (client, config)
    }
    fn photo(client: &Client, dir: &tempfile::TempDir) -> AttachmentId {
        let path = dir.path().join(format!("{}.png", uuid::Uuid::new_v4()));
        image::RgbImage::from_pixel(40, 30, image::Rgb([9, 99, 199]))
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        client.prepare_contact_photo(&path).unwrap().attachment_id
    }
    fn capture(client: &Client, config: &ClientConfig, id: AttachmentId) {
        let input = json!({"schema_version": 1, "book": {"id": "b", "owner_device_id": config.device_id.to_string(), "generation": "1", "state": "active"},
            "contacts": [{"id": "c", "book_id": "b", "display_name": "Ada", "photo": {"attachment_id": id.to_string()}}]});
        client.capture_contact_book(&input.to_string()).unwrap();
    }
    fn exec(client: &Client, sql: &str, id: AttachmentId) {
        client
            .lock()
            .unwrap()
            .conn
            .execute(sql, [id.to_string()])
            .unwrap();
    }
    fn held_plain_mentions(client: &Client, id: AttachmentId) -> bool {
        client.lock().unwrap().conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM outbox WHERE state='unsealed' AND instr(CAST(plain AS TEXT), ?)>0)",
            [id.to_string()],
            |r| r.get(0),
        ).unwrap()
    }
    fn state(client: &Client) -> Value {
        serde_json::from_str(&client.contact_photo_transfer_state_json().unwrap()).unwrap()
    }

    /// An attachment row deleted out-of-band never turns "set photo" into "no photo" on the wire.
    #[test]
    fn missing_photo_row_holds_and_reports_instead_of_dropping() {
        let dir = tempfile::TempDir::new().unwrap();
        let (client, config) = client(&dir);
        let id = photo(&client, &dir);
        capture(&client, &config, id);
        exec(&client, "DELETE FROM attachments WHERE attachment_id=?", id);
        client.seal_pending_batch(100).unwrap();
        assert!(
            held_plain_mentions(&client, id),
            "photo intent kept in the held row"
        );
        // Only the book state is sealed; the photo-bearing upsert is not published photo-less.
        assert_eq!(client.pending_outbox().unwrap().len(), 1);
        let unavailable = state(&client)["unavailable"].clone();
        assert_eq!(unavailable.as_array().unwrap().len(), 1);
        assert_eq!(unavailable[0]["local_attachment_id"], id.to_string());
        assert_eq!(unavailable[0]["reason"], "missing");
    }

    /// A held row whose photo's remote object was released meanwhile is never sealed with it.
    #[test]
    fn released_remote_object_is_never_sealed() {
        let dir = tempfile::TempDir::new().unwrap();
        let (client, config) = client(&dir);
        let id = photo(&client, &dir);
        capture(&client, &config, id);
        let remote = uuid::Uuid::new_v4().to_string();
        client
            .lock()
            .unwrap()
            .conn
            .execute_batch(&format!(
                "UPDATE attachments SET state='uploaded',remote_object_id='{remote}' WHERE attachment_id='{id}';
                 INSERT INTO contact_photo_reclaims(attachment_id,remote_object_id,state,attempts,next_due_at) VALUES('{id}','{remote}','completed',0,0);"
            ))
            .unwrap();
        client.seal_pending_batch(100).unwrap();
        assert!(held_plain_mentions(&client, id));
        assert_eq!(client.pending_outbox().unwrap().len(), 1);
        assert_eq!(state(&client)["unavailable"][0]["reason"], "reclaimed");
        assert_eq!(state(&client)["registrations"], json!([]));
    }

    /// 409 never abandons a candidate: retries back off 1h, 2h, 4h ... capped daily, so a
    /// 30-day replay / 90-day restore horizon is retried until the server releases it.
    #[test]
    fn conflict_reclaims_back_off_daily_and_are_retried_until_released() {
        let dir = tempfile::TempDir::new().unwrap();
        let (client, config) = client(&dir);
        let own = config.device_id.to_string();
        let id = photo(&client, &dir);
        exec(
            &client,
            "UPDATE attachments SET state='uploaded',remote_object_id='0b8f6a54-2a4d-4f1e-9a43-3f7f5c3c1a11' WHERE attachment_id=?",
            id,
        );
        let s = client.lock().unwrap();
        let conn = &s.conn;
        let at = |now: i64| -> Value {
            serde_json::from_str(&transfer_state_at(conn, &own, now).unwrap()).unwrap()
        };
        let conflict =
            json!({"schema_version": 1, "attachment_id": id.to_string(), "http_status": 409})
                .to_string();
        let mut now = 1_800_000_000;
        assert_eq!(at(now)["reclaims"][0]["attachment_id"], id.to_string());
        let mut delays = Vec::new();
        // 100 conflicts span well past 90 days of daily retries.
        for _ in 0..100 {
            let out: Value =
                serde_json::from_str(&acknowledge_reclaim_at(conn, &conflict, now).unwrap())
                    .unwrap();
            assert_eq!(out["status"], "pending");
            let due = out["next_due_at"].as_i64().unwrap();
            delays.push(due - now);
            let deferred = at(due - 1);
            assert_eq!(deferred["reclaims"], json!([]));
            assert_eq!(deferred["reclaims_deferred"], 1);
            now = due;
            assert_eq!(
                at(now)["reclaims"][0]["attachment_id"],
                id.to_string(),
                "offered again when due"
            );
        }
        assert_eq!(&delays[..6], &[3600, 7200, 14400, 28800, 57600, 86400]);
        assert!(delays.iter().all(|d| *d <= 86400));
        assert!(now - 1_800_000_000 > 90 * 86400);
        assert!(
            blocks_discard(conn, id).unwrap(),
            "retained while unreleased"
        );
        let released =
            json!({"schema_version": 1, "attachment_id": id.to_string(), "http_status": 204})
                .to_string();
        let out: Value =
            serde_json::from_str(&acknowledge_reclaim_at(conn, &released, now).unwrap()).unwrap();
        assert_eq!(out["status"], "completed");
        assert_eq!(at(now + 10 * 86400)["reclaims"], json!([]));
        assert!(!blocks_discard(conn, id).unwrap());
    }

    /// Two vault members, authenticated envelopes relayed by hand.
    fn pair(dir: &tempfile::TempDir) -> (Client, ClientConfig, Client) {
        let vault_id = VaultId::new();
        let profile = KeyProfile::new(vault_id.0, 1).unwrap();
        let root = derive_root_key(PASS, &profile).unwrap();
        let header = create_vault_check_header(&root, profile.clone()).unwrap();
        let open = |name: &str| {
            let config = ClientConfig {
                database_path: dir.path().join(format!("{name}.db")),
                vault_id,
                device_id: DeviceId::new(),
            };
            let client = Client::open(config.clone(), DatabaseKey::new(&[4; 32]).unwrap()).unwrap();
            client.unlock(&profile, &header, PASS).unwrap();
            client.set_server_compaction_state(true, true).unwrap();
            (client, config)
        };
        let (sender, sender_cfg) = open("sender");
        let (receiver, _) = open("receiver");
        (sender, sender_cfg, receiver)
    }

    /// A vault member that lies: a generic attachment labelled `image/jpeg` (<= 64 KiB) is
    /// forced into the contact-photo set and published with a valid descriptor.
    fn publish_lying_photo(
        sender: &Client,
        config: &ClientConfig,
        receiver: &Client,
        dir: &tempfile::TempDir,
        bytes: &[u8],
        cursor: &mut u64,
    ) -> AttachmentId {
        let path = dir.path().join(format!("{}.bin", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes).unwrap();
        let id = sender
            .prepare_attachment(&path, "image/jpeg", "contact_photo.jpg")
            .unwrap()
            .attachment_id;
        exec(
            sender,
            "INSERT INTO contact_photo_attachments(attachment_id) VALUES(?)",
            id,
        );
        capture(sender, config, id);
        sender
            .mark_attachment_uploaded(id, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        for r in state(sender)["registrations"].as_array().unwrap() {
            sender
                .acknowledge_contact_photo_reference_json(
                    &json!({"schema_version": 1, "envelope_id": r["envelope_id"], "attachment_id": r["attachment_id"]}).to_string(),
                )
                .unwrap();
        }
        for envelope in sender.pending_outbox().unwrap() {
            *cursor += 1;
            receiver.ingest(&envelope, crate::Cursor(*cursor)).unwrap();
            sender.ack_outbox(envelope.envelope_id).unwrap();
        }
        let report = receiver.apply_pending(100).unwrap();
        assert_eq!(report.quarantined, 0, "descriptor itself is well-formed");
        id
    }

    fn view_photo(client: &Client) -> Value {
        let v: Value = serde_json::from_str(
            &client
                .contact_book_view(&json!({"schema_version": 1, "book_id": "b"}).to_string())
                .unwrap(),
        )
        .unwrap();
        v["contacts"][0]["photo"].clone()
    }

    /// Received contact photos are verified by content at install: a lying descriptor (wrong
    /// format, wrong dimensions, decompression-bomb header) never becomes available.
    #[test]
    fn received_photo_bytes_are_verified_before_availability() {
        let dir = tempfile::TempDir::new().unwrap();
        let (sender, config, receiver) = pair(&dir);
        let mut cursor = 0;
        let jpeg = |w: u32, h: u32| {
            let mut out = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 50)
                .encode_image(&image::RgbImage::from_pixel(w, h, image::Rgb([1, 2, 3])))
                .unwrap();
            out
        };
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                &[7u8; 256 * 256 * 3],
                256,
                256,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        // JPEG SOF0 rewritten to declare 60000x60000 (a decompression bomb header).
        let mut bomb = jpeg(256, 256);
        let sof = bomb.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        bomb[sof + 5..sof + 9].copy_from_slice(&[0xEA, 0x60, 0xEA, 0x60]);
        let cases: [(&str, Vec<u8>); 3] = [
            ("png labelled jpeg", png),
            ("512x512 jpeg", jpeg(512, 512)),
            ("bomb header", bomb),
        ];
        for (name, bytes) in cases {
            assert!(bytes.len() <= 64 * 1024, "{name}");
            let id = publish_lying_photo(&sender, &config, &receiver, &dir, &bytes, &mut cursor);
            assert!(
                receiver
                    .pending_downloads()
                    .unwrap()
                    .iter()
                    .any(|o| o.attachment_id == id),
                "{name}"
            );
            let fetched = dir.path().join("fetched.ppss");
            std::fs::copy(sender.native_cipher_file(id).unwrap(), &fetched).unwrap();
            // Ciphertext verifies (authentic), the content does not.
            assert!(
                matches!(
                    receiver.install_downloaded_attachment(id, &fetched),
                    Err(Error::InvalidMedia)
                ),
                "{name}"
            );
            assert!(receiver.open_native_plaintext(id).is_err(), "{name}");
            assert!(receiver.native_cipher_file(id).is_err(), "{name}");
            assert_eq!(view_photo(&receiver)["available"], false, "{name}");
            assert!(
                !receiver
                    .pending_downloads()
                    .unwrap()
                    .iter()
                    .any(|o| o.attachment_id == id),
                "{name}: no re-download loop"
            );
            let media_root = dir.path().join("receiver.db.media");
            assert!(!media::cipher_path(&media_root, id).exists(), "{name}");
        }
        // Control: a genuinely normalized photo installs and becomes available.
        let good = photo(&sender, &dir);
        capture(&sender, &config, good);
        sender
            .mark_attachment_uploaded(good, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        for r in state(&sender)["registrations"].as_array().unwrap() {
            sender
                .acknowledge_contact_photo_reference_json(
                    &json!({"schema_version": 1, "envelope_id": r["envelope_id"], "attachment_id": r["attachment_id"]}).to_string(),
                )
                .unwrap();
        }
        for envelope in sender.pending_outbox().unwrap() {
            cursor += 1;
            receiver.ingest(&envelope, crate::Cursor(cursor)).unwrap();
            sender.ack_outbox(envelope.envelope_id).unwrap();
        }
        receiver.apply_pending(100).unwrap();
        let fetched = dir.path().join("good.ppss");
        std::fs::copy(sender.native_cipher_file(good).unwrap(), &fetched).unwrap();
        receiver
            .install_downloaded_attachment(good, &fetched)
            .unwrap();
        assert_eq!(
            view_photo(&receiver),
            json!({"attachment_id": good.to_string(), "available": true})
        );
    }

    /// Generic attachments (e.g. MMS JPEGs) cannot be referenced as contact photos.
    #[test]
    fn generic_image_attachment_is_not_a_contact_photo() {
        let dir = tempfile::TempDir::new().unwrap();
        let (client, config) = client(&dir);
        let path = dir.path().join("mms.jpg");
        image::RgbImage::from_pixel(10, 10, image::Rgb([1, 1, 1]))
            .save_with_format(&path, image::ImageFormat::Jpeg)
            .unwrap();
        let id = client
            .prepare_attachment(&path, "image/jpeg", "mms.jpg")
            .unwrap()
            .attachment_id;
        let input = json!({"schema_version": 1, "book": {"id": "b", "owner_device_id": config.device_id.to_string(), "generation": "1", "state": "active"},
            "contacts": [{"id": "c", "book_id": "b", "display_name": "Ada", "photo": {"attachment_id": id.to_string()}}]});
        assert!(matches!(
            client.capture_contact_book(&input.to_string()),
            Err(Error::InvalidRequest(_))
        ));
    }
}
