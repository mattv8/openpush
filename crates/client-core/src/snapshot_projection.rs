//! Authoritative projection of compactable state from a server compaction snapshot.
//!
//! Legacy snapshots (no server compaction generation) only merge history monotonically. A
//! compaction snapshot is different: the server has dropped superseded records and purged
//! expired terminal tombstones, so a merge would leave a removed contact or notification
//! visible forever on a device that was offline when the tombstone existed. For such a
//! generation:
//!
//! 1. `drain_snapshot` records the cursor of every retained record that carries compaction
//!    metadata (`projection_retained`), including records that were already journaled.
//! 2. Once the drain finishes, `advance` decrypts those journaled records in bounded,
//!    resumable batches and reduces them into generation-keyed stage tables with the same
//!    semantic gates as live apply: contact revision and book generation, notification source
//!    sequence plus lifetime tombstones, and app-filter `(revision, writer)` LWW. Edit
//!    requests/results, dismissals and commands are never staged and get no effects here.
//! 3. When every retained record is staged, `promote` reconciles remote-owned rows against the
//!    stage inside the caller's transaction: rows absent from the stage are removed and staged
//!    rows merge through the same gates. Books owned by this device, own-source
//!    notifications, contact sources, the edit ledger/permits and unsent filter writes are
//!    never touched, so the owner can still rescan its OS provider.
//! 4. Journal rows at or below the snapshot high-water that carry compaction metadata but are
//!    not retained are suppressed (`duplicate` / `snapshot_superseded`) instead of applied, so
//!    a pending pre-snapshot record cannot resurrect removed state. Non-compactable records
//!    (messages, statuses, commands) are unaffected.
//!
//! Until promotion the apply ceiling stays at the snapshot high-water, so nothing newer can be
//! applied and then erased. Nothing visible is cleared before promotion. A retained record that
//! cannot be authenticated or fails payload validation fails the projection: live state stays
//! as it was and the reason is reported by `Client::snapshot_projection_status`. Records that
//! validate but are not authorized (a foreign device claiming a book) are ignored exactly as
//! live apply ignores them by quarantine.
use crate::{
    AppFilter, Client, Ctx, Cursor, Error, MAX_SNAPSHOT_PAGE_BYTES, NotificationTarget,
    NotificationWire, Opened, PrivatePayload, contact_media, contacts, notifications, open_payload,
    parse_wire, take_within_budget, to_i64,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

/// Stage rows removed per maintenance step.
const GC_BATCH: i64 = 2_000;
/// Rows read per page while promoting (photo holders, filters).
const PROMOTE_PAGE: i64 = 256;
/// Journal reason for compactable rows the authoritative snapshot proved superseded.
pub(crate) const SUPERSEDED_REASON: &str = "snapshot_superseded";

/// Metadata latch: this device's contact/notification projection must be rebuilt from an
/// authoritative compaction snapshot. Value is the reason (`upgrade` or `requested`).
const REPAIR_LATCH: &str = "contact_repair_required";

pub(crate) fn initialize(conn: &Connection) -> Result<(), Error> {
    // An existing database gaining these tables predates authoritative projection: latch one
    // repair so native fetches a snapshot instead of republishing every owned contact.
    // Fresh databases have no `metadata` table yet and need no repair.
    let table_exists = |name: &str| -> Result<bool, Error> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
            [name],
            |r| r.get(0),
        )?)
    };
    let upgrading = table_exists("metadata")? && !table_exists("projection_generations")?;
    conn.execute_batch(
        "\
CREATE TABLE IF NOT EXISTS projection_generations(generation INTEGER PRIMARY KEY, high_water INTEGER NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('draining','staging','promoted','failed','superseded')),
  progress INTEGER NOT NULL DEFAULT 0, reason TEXT);
CREATE TABLE IF NOT EXISTS projection_retained(generation INTEGER NOT NULL, cursor INTEGER NOT NULL, PRIMARY KEY(generation,cursor));
CREATE TABLE IF NOT EXISTS projection_books(generation INTEGER NOT NULL, book_id TEXT NOT NULL, owner_device_id TEXT NOT NULL,
  book_generation INTEGER NOT NULL, state TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(generation,book_id));
CREATE TABLE IF NOT EXISTS projection_contacts(generation INTEGER NOT NULL, book_id TEXT NOT NULL, contact_id TEXT NOT NULL,
  revision INTEGER NOT NULL, body TEXT NOT NULL, deleted_at INTEGER, PRIMARY KEY(generation,book_id,contact_id));
CREATE TABLE IF NOT EXISTS projection_notifications(generation INTEGER NOT NULL, source_device_id TEXT NOT NULL,
  notification_key TEXT NOT NULL, lifetime TEXT NOT NULL, instance TEXT NOT NULL, package_name TEXT NOT NULL,
  app_name TEXT NOT NULL, title TEXT NOT NULL, text TEXT NOT NULL, category TEXT, posted_at INTEGER NOT NULL,
  dismissible INTEGER NOT NULL, removed INTEGER NOT NULL, source_sequence INTEGER NOT NULL,
  PRIMARY KEY(generation,source_device_id,notification_key));
CREATE TABLE IF NOT EXISTS projection_tombstones(generation INTEGER NOT NULL, source_device_id TEXT NOT NULL,
  notification_key TEXT NOT NULL, lifetime TEXT NOT NULL, source_sequence INTEGER NOT NULL,
  PRIMARY KEY(generation,source_device_id,notification_key,lifetime));
CREATE TABLE IF NOT EXISTS projection_filters(generation INTEGER NOT NULL, source_device_id TEXT NOT NULL,
  package_name TEXT NOT NULL, app_name TEXT NOT NULL, muted INTEGER NOT NULL, source_sequence INTEGER NOT NULL,
  producer_device_id TEXT NOT NULL, logical_revision INTEGER NOT NULL, writer_device_id TEXT NOT NULL,
  PRIMARY KEY(generation,source_device_id,package_name));
",
    )?;
    if upgrading {
        conn.execute(
            "INSERT OR IGNORE INTO metadata(k,v) VALUES(?,'upgrade')",
            [REPAIR_LATCH],
        )?;
    }
    Ok(())
}

/// Public view of the newest authoritative projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotProjectionState {
    /// Snapshot records are still moving into the journal.
    Draining,
    /// Retained records are being authenticated and staged; live state is unchanged.
    Staging,
    /// The snapshot replaced remote compactable state.
    Promoted,
    /// Not promoted; live state was left as it was. See `reason`.
    Failed,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotProjectionStatus {
    pub generation: u64,
    pub high_water: Cursor,
    pub state: SnapshotProjectionState,
    /// Why a projection failed: a quarantine reason code, `keys_unavailable`,
    /// `stale_snapshot` or `missing_record`.
    pub reason: Option<String>,
}

impl Client {
    /// Whether local contact/notification projection state needs an authoritative rebuild.
    /// While true, native sync should fetch a compaction snapshot (`begin_snapshot_with_compaction`)
    /// rather than republish owned data; the latch clears only when such a snapshot promotes.
    pub fn contact_repair_required(&self) -> Result<bool, Error> {
        Ok(crate::get_meta(&self.lock()?.conn, REPAIR_LATCH)?.is_some())
    }

    /// Reports local projection damage (e.g. an integrity warning) and latches a repair. An
    /// existing reason is kept. Live state is not cleared; the next promotion replaces it.
    pub fn request_contact_repair(&self) -> Result<(), Error> {
        self.lock()?.conn.execute(
            "INSERT OR IGNORE INTO metadata(k,v) VALUES(?,'requested')",
            [REPAIR_LATCH],
        )?;
        Ok(())
    }

    /// The newest compaction-snapshot projection, if any. `Failed` means the snapshot did not
    /// become authoritative (e.g. a retained record could not be authenticated, or an older
    /// key epoch is not unlocked); fetch a new snapshot after resolving the reason.
    pub fn snapshot_projection_status(&self) -> Result<Option<SnapshotProjectionStatus>, Error> {
        let s = self.lock()?;
        let row: Option<(i64, i64, String, Option<String>)> = s
            .conn
            .query_row(
                "SELECT generation,high_water,state,reason FROM projection_generations WHERE state!='superseded' ORDER BY generation DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((generation, high_water, state, reason)) = row else {
            return Ok(None);
        };
        let u = |v: i64| u64::try_from(v).map_err(|_| Error::Database);
        Ok(Some(SnapshotProjectionStatus {
            generation: u(generation)?,
            high_water: Cursor(u(high_water)?),
            state: match state.as_str() {
                "draining" => SnapshotProjectionState::Draining,
                "staging" => SnapshotProjectionState::Staging,
                "promoted" => SnapshotProjectionState::Promoted,
                "failed" => SnapshotProjectionState::Failed,
                _ => return Err(Error::Database),
            },
            reason,
        }))
    }
}

fn compactable(wire: &[u8]) -> bool {
    parse_wire(wire).is_some_and(|e| e.compaction.is_some())
}

/// `finish_snapshot` hook (same transaction). A newly published generation makes every older
/// unpromoted projection unpromotable; a compaction generation starts its own projection.
pub(crate) fn on_publish(
    conn: &Connection,
    generation: i64,
    high_water: i64,
    authoritative: bool,
    records: i64,
) -> Result<(), Error> {
    conn.execute(
        "UPDATE projection_generations SET state='superseded' WHERE state IN ('draining','staging','failed')",
        [],
    )?;
    if authoritative {
        // A zero-record compaction snapshot has nothing to drain: every remote compactable
        // row is gone, which promotion applies on the next `apply_pending`.
        conn.execute(
            "INSERT INTO projection_generations(generation,high_water,state) VALUES(?,?,?)",
            params![
                generation,
                high_water,
                if records == 0 { "staging" } else { "draining" }
            ],
        )?;
    }
    Ok(())
}

/// Whether `drain_snapshot` must record retained cursors for this generation.
pub(crate) fn is_draining(conn: &Connection, generation: i64) -> Result<bool, Error> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM projection_generations WHERE generation=? AND state='draining'",
            params![generation],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Records a drained snapshot cursor when it carries compaction metadata.
pub(crate) fn retain(
    conn: &Connection,
    generation: i64,
    cursor: i64,
    wire: &[u8],
) -> Result<(), Error> {
    if compactable(wire) {
        conn.execute(
            "INSERT OR IGNORE INTO projection_retained(generation,cursor) VALUES(?,?)",
            params![generation, cursor],
        )?;
    }
    Ok(())
}

/// `drain_snapshot` hook once the generation's last record reached the journal.
pub(crate) fn drained(conn: &Connection, generation: i64) -> Result<(), Error> {
    conn.execute(
        "UPDATE projection_generations SET state='staging' WHERE generation=? AND state='draining'",
        params![generation],
    )?;
    Ok(())
}

fn fail(conn: &Connection, generation: i64, reason: &str) -> Result<(), Error> {
    conn.execute(
        "UPDATE projection_generations SET state='failed',reason=? WHERE generation=?",
        params![reason, generation],
    )?;
    Ok(())
}

/// Stages up to `limit` retained records of the newest unpromoted projection and promotes it
/// when none remain. Returns the apply ceiling (the snapshot high-water) while the projection
/// is unpromoted, `None` otherwise. Staged records count as `drained` so callers keep calling;
/// while the vault is locked nothing is staged and nothing is reported as remaining.
pub(crate) fn advance(
    conn: &Connection,
    ctx: &Ctx,
    limit: usize,
    report: &mut crate::ApplyReport,
) -> Result<Option<i64>, Error> {
    let row: Option<(i64, i64, String, i64)> = conn
        .query_row(
            "SELECT generation,high_water,state,progress FROM projection_generations WHERE state IN ('draining','staging') ORDER BY generation DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((generation, high_water, state, mut progress)) = row else {
        return Ok(None);
    };
    if state == "draining" {
        return Ok(Some(high_water));
    }
    // Rows newer than the snapshot were applied before it was requested; replacing state
    // with the older snapshot would erase them.
    let stale = conn
        .query_row(
            "SELECT 1 FROM journal WHERE cursor>? AND status='applied' LIMIT 1",
            params![high_water],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if stale {
        fail(conn, generation, "stale_snapshot")?;
        return Ok(None);
    }
    let rows: Vec<(i64, Option<Vec<u8>>)> = {
        let mut query = conn.prepare(
            "SELECT COALESCE(LENGTH(j.wire),0),r.cursor,j.wire FROM projection_retained r LEFT JOIN journal j ON j.cursor=r.cursor WHERE r.generation=? AND r.cursor>? ORDER BY r.cursor LIMIT ?",
        )?;
        take_within_budget(
            query.query(params![generation, progress, to_i64(limit as u64)?])?,
            MAX_SNAPSHOT_PAGE_BYTES,
            |r| Ok((r.get(1)?, r.get(2)?)),
        )?
    };
    let mut staged = 0usize;
    for (cursor, wire) in rows {
        let Some(envelope) = wire.as_deref().and_then(parse_wire) else {
            fail(conn, generation, "missing_record")?;
            return Ok(None);
        };
        if envelope.validate().is_err() || envelope.vault_id != ctx.vault_id {
            fail(conn, generation, "malformed_envelope")?;
            return Ok(None);
        }
        if !ctx.keys.contains_key(&envelope.key_epoch) {
            if ctx.keys.is_empty() {
                // Locked: resume after unlock without claiming remaining work.
                break;
            }
            fail(conn, generation, "keys_unavailable")?;
            return Ok(None);
        }
        let payload = match open_payload(ctx, &envelope) {
            Ok(Opened::Payload(payload)) => Some(payload),
            Ok(Opened::UnknownEvent) => None,
            Err(reason) => {
                fail(conn, generation, reason.code())?;
                return Ok(None);
            }
        };
        if let Some(payload) = payload {
            let staged_ok = stage(
                conn,
                generation,
                &ctx.device_id.to_string(),
                &envelope.producer_device_id.to_string(),
                envelope.producer_sequence.0,
                payload,
            )?;
            if !staged_ok {
                fail(conn, generation, "invalid_payload")?;
                return Ok(None);
            }
        }
        progress = cursor;
        staged += 1;
    }
    conn.execute(
        "UPDATE projection_generations SET progress=? WHERE generation=?",
        params![progress, generation],
    )?;
    report.drained += staged;
    let remaining: i64 = conn.query_row(
        "SELECT COUNT(*) FROM projection_retained WHERE generation=? AND cursor>?",
        params![generation, progress],
        |r| r.get(0),
    )?;
    if remaining > 0 {
        if !ctx.keys.is_empty() {
            report.snapshot_remaining += u64::try_from(remaining).map_err(|_| Error::Database)?;
        }
        return Ok(Some(high_water));
    }
    promote(conn, generation, high_water, &ctx.device_id.to_string())?;
    Ok(None)
}

/// Reduces one authenticated retained payload. `Ok(false)` means it failed validation.
fn stage(
    conn: &Connection,
    generation: i64,
    me: &str,
    producer: &str,
    sequence: u64,
    payload: PrivatePayload,
) -> Result<bool, Error> {
    // Shape errors from the shared contact/media validators are validation failures, like
    // live apply's quarantine; anything else is a real database error.
    match reduce(conn, generation, me, producer, sequence, payload) {
        Err(Error::InvalidRequest(_)) => Ok(false),
        other => other,
    }
}

fn reduce(
    conn: &Connection,
    generation: i64,
    me: &str,
    producer: &str,
    sequence: u64,
    payload: PrivatePayload,
) -> Result<bool, Error> {
    let Ok(sequence) = i64::try_from(sequence) else {
        return Ok(false);
    };
    match payload {
        // Own contact books are local truth, rebuilt by rescanning the OS provider.
        PrivatePayload::ContactBookState { .. }
        | PrivatePayload::ContactUpserted { .. }
        | PrivatePayload::ContactRemoved { .. }
            if producer == me =>
        {
            Ok(true)
        }
        payload @ (PrivatePayload::ContactBookState { .. }
        | PrivatePayload::ContactUpserted { .. }
        | PrivatePayload::ContactRemoved { .. }) => {
            // Same receive-side photo rewriting as live apply (descriptor -> local reference).
            let Some(payload) = contact_media::ingest(conn, payload)? else {
                return Ok(false);
            };
            stage_contact_payload(conn, generation, producer, &payload)
        }
        PrivatePayload::NotificationPosted { notification } => {
            if !notifications::valid_wire(&notification)
                || notification.target.source_device_id != producer
            {
                return Ok(false);
            }
            if producer != me {
                stage_post(conn, generation, &notification, sequence)?;
            }
            Ok(true)
        }
        PrivatePayload::NotificationRemoved { target, instance } => {
            if !notifications::valid_target(&target)
                || !notifications::valid(&instance)
                || target.source_device_id != producer
            {
                return Ok(false);
            }
            if producer != me {
                stage_removal(conn, generation, &target, sequence)?;
            }
            Ok(true)
        }
        PrivatePayload::AppFilter {
            filter,
            logical_revision,
            writer_device_id,
        } => {
            if !notifications::valid_filter_event(
                &filter,
                logical_revision,
                &writer_device_id,
                producer,
            ) {
                return Ok(false);
            }
            conn.execute(
                "INSERT INTO projection_filters(generation,source_device_id,package_name,app_name,muted,source_sequence,producer_device_id,logical_revision,writer_device_id) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(generation,source_device_id,package_name) DO UPDATE SET app_name=excluded.app_name,muted=excluded.muted,source_sequence=excluded.source_sequence,producer_device_id=excluded.producer_device_id,logical_revision=excluded.logical_revision,writer_device_id=excluded.writer_device_id WHERE excluded.logical_revision>projection_filters.logical_revision OR (excluded.logical_revision=projection_filters.logical_revision AND excluded.writer_device_id>projection_filters.writer_device_id)",
                params![generation, filter.source_device_id, filter.package_name, filter.app_name, filter.muted, sequence, producer, to_i64(logical_revision)?, writer_device_id],
            )?;
            Ok(true)
        }
        // Edit requests/results, dismissals and unknown kinds carry no staged state.
        _ => Ok(true),
    }
}

/// Owner of `book_id` as staged in this generation, else as currently materialized.
fn known_owner(conn: &Connection, generation: i64, book_id: &str) -> Result<Option<String>, Error> {
    let staged: Option<String> = conn
        .query_row(
            "SELECT owner_device_id FROM projection_books WHERE generation=? AND book_id=?",
            params![generation, book_id],
            |r| r.get(0),
        )
        .optional()?;
    if staged.is_some() {
        return Ok(staged);
    }
    Ok(conn
        .query_row(
            "SELECT owner_device_id FROM contact_books WHERE id=?",
            [book_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Mirrors `contacts::apply_book`/`apply_peer` against the stage. `Ok(true)` also covers
/// authorized-but-ignored records (foreign owner), which live apply quarantines without effect.
fn stage_contact_payload(
    conn: &Connection,
    generation: i64,
    producer: &str,
    payload: &PrivatePayload,
) -> Result<bool, Error> {
    match payload {
        PrivatePayload::ContactBookState { book } => {
            stage_book(conn, generation, producer, book)?;
            Ok(true)
        }
        PrivatePayload::ContactUpserted { book, contact } => {
            if !stage_book(conn, generation, producer, book)? {
                return Ok(true);
            }
            let book_id = contacts::req(book, "id")?;
            let (id, revision, body) = contacts::peer_contact(contact, book_id)?;
            conn.execute(
                "INSERT INTO projection_contacts(generation,book_id,contact_id,revision,body,deleted_at) VALUES(?,?,?,?,?,NULL) ON CONFLICT(generation,book_id,contact_id) DO UPDATE SET revision=excluded.revision,body=excluded.body,deleted_at=NULL WHERE excluded.revision>projection_contacts.revision",
                params![generation, book_id, id, revision, crate::json(&body)?],
            )?;
            Ok(true)
        }
        PrivatePayload::ContactRemoved {
            book_id,
            contact_id,
            tombstone,
        } => {
            if contacts::req(tombstone, "contact_id")? != contact_id {
                return Ok(false);
            }
            // Self-contained tombstones stage their book first (compacted [removal, book]).
            if let Some(book) = tombstone.get("book") {
                if contacts::req(book, "id")? != book_id {
                    return Ok(false);
                }
                if !stage_book(conn, generation, producer, book)? {
                    return Ok(true);
                }
            }
            let revision = contacts::revision(tombstone, "revision")?;
            let Some(deleted_at) = tombstone.get("deleted_at").and_then(Value::as_i64) else {
                return Ok(false);
            };
            let Some(restored) = tombstone.get("restored_contact") else {
                return Ok(false);
            };
            let (id, body_revision, body) = contacts::peer_contact(restored, book_id)?;
            if id != *contact_id || body_revision != revision {
                return Ok(false);
            }
            if known_owner(conn, generation, book_id)?.as_deref() != Some(producer) {
                return Ok(true);
            }
            conn.execute(
                "INSERT INTO projection_contacts(generation,book_id,contact_id,revision,body,deleted_at) VALUES(?,?,?,?,?,?) ON CONFLICT(generation,book_id,contact_id) DO UPDATE SET revision=excluded.revision,body=excluded.body,deleted_at=excluded.deleted_at WHERE excluded.revision>projection_contacts.revision",
                params![generation, book_id, id, revision, crate::json(&body)?, deleted_at],
            )?;
            Ok(true)
        }
        _ => Ok(true),
    }
}

/// Stages book state. `Ok(false)`: the producer does not own the book (ignored record).
fn stage_book(
    conn: &Connection,
    generation: i64,
    producer: &str,
    book: &Value,
) -> Result<bool, Error> {
    let id = contacts::req(book, "id")?;
    if contacts::req(book, "owner_device_id")? != producer {
        return Ok(false);
    }
    let book_generation = contacts::revision(book, "generation")?;
    if known_owner(conn, generation, id)?.is_some_and(|owner| owner != producer) {
        return Ok(false);
    }
    let staged: Option<(i64, String)> = conn
        .query_row(
            "SELECT book_generation,body FROM projection_books WHERE generation=? AND book_id=?",
            params![generation, id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    // Same rule as live apply: record order is not authoritative within a generation.
    if let Some((staged_generation, staged_body)) = staged {
        let staged_body: Value = serde_json::from_str(&staged_body).map_err(|_| Error::Database)?;
        if !contacts::book_supersedes(staged_generation, &staged_body, book_generation, book)? {
            return Ok(true);
        }
    } else {
        contacts::book_state_revision(book)?;
    }
    let mut body = book.clone();
    contacts::strip_sources(&mut body);
    conn.execute(
        "INSERT INTO projection_books(generation,book_id,owner_device_id,book_generation,state,body) VALUES(?,?,?,?,?,?) ON CONFLICT(generation,book_id) DO UPDATE SET book_generation=excluded.book_generation,state=excluded.state,body=excluded.body",
        params![
            generation,
            id,
            producer,
            book_generation,
            book.get("state").and_then(Value::as_str).unwrap_or("active"),
            crate::json(&body)?
        ],
    )?;
    Ok(true)
}

/// Mirrors `notifications::upsert_notification` (lifetime tombstones are permanent, newer
/// source sequence wins) against the stage.
fn stage_post(
    conn: &Connection,
    generation: i64,
    n: &NotificationWire,
    sequence: i64,
) -> Result<(), Error> {
    let t = &n.target;
    let tombstoned = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM projection_tombstones WHERE generation=?1 AND source_device_id=?2 AND notification_key=?3 AND lifetime=?4) OR EXISTS(SELECT 1 FROM notification_tombstones WHERE source_device_id=?2 AND notification_key=?3 AND lifetime=?4)",
            params![generation, t.source_device_id, t.notification_key, t.lifetime],
            |r| r.get::<_, bool>(0),
        )?;
    if tombstoned {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO projection_notifications(generation,source_device_id,notification_key,lifetime,instance,package_name,app_name,title,text,category,posted_at,dismissible,removed,source_sequence) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,0,?) ON CONFLICT(generation,source_device_id,notification_key) DO UPDATE SET lifetime=excluded.lifetime,instance=excluded.instance,package_name=excluded.package_name,app_name=excluded.app_name,title=excluded.title,text=excluded.text,category=excluded.category,posted_at=excluded.posted_at,dismissible=excluded.dismissible,removed=0,source_sequence=excluded.source_sequence WHERE excluded.source_sequence>projection_notifications.source_sequence",
        params![generation, t.source_device_id, t.notification_key, t.lifetime, n.instance, n.package_name, n.app_name, n.title, n.text, n.category, n.posted_at, n.dismissible, sequence],
    )?;
    Ok(())
}

/// Mirrors `notifications::remove_applied` against the stage.
fn stage_removal(
    conn: &Connection,
    generation: i64,
    t: &NotificationTarget,
    sequence: i64,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO projection_tombstones(generation,source_device_id,notification_key,lifetime,source_sequence) VALUES(?,?,?,?,?) ON CONFLICT(generation,source_device_id,notification_key,lifetime) DO UPDATE SET source_sequence=MAX(projection_tombstones.source_sequence,excluded.source_sequence)",
        params![generation, t.source_device_id, t.notification_key, t.lifetime, sequence],
    )?;
    conn.execute(
        "UPDATE projection_notifications SET removed=1,title='',text='',source_sequence=? WHERE generation=? AND source_device_id=? AND notification_key=? AND lifetime=? AND source_sequence<=?",
        params![sequence, generation, t.source_device_id, t.notification_key, t.lifetime, sequence],
    )?;
    Ok(())
}

/// Reconciles remote-owned compactable rows against the complete stage (caller's
/// transaction). Absent remote rows are removed; staged rows merge through the live gates so a
/// snapshot can never roll a revision, generation or source sequence back.
fn promote(conn: &Connection, generation: i64, high_water: i64, me: &str) -> Result<(), Error> {
    // Contact books owned elsewhere; deleting a book cascades to its contacts.
    conn.execute(
        "DELETE FROM contact_books WHERE owner_device_id!=?1 AND NOT EXISTS(SELECT 1 FROM projection_books s WHERE s.generation=?2 AND s.book_id=contact_books.id)",
        params![me, generation],
    )?;
    conn.execute(
        "INSERT INTO contact_books(id,owner_device_id,generation,state,body,forgotten) SELECT book_id,owner_device_id,book_generation,state,body,0 FROM projection_books WHERE generation=?2 AND owner_device_id!=?1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,state=excluded.state,body=excluded.body WHERE contact_books.owner_device_id=excluded.owner_device_id AND (excluded.generation>contact_books.generation OR (excluded.generation=contact_books.generation AND contact_books.state!='retired' AND CAST(COALESCE(json_extract(excluded.body,'$.state_revision'),'0') AS INTEGER)>=CAST(COALESCE(json_extract(contact_books.body,'$.state_revision'),'0') AS INTEGER)))",
        params![me, generation],
    )?;
    conn.execute(
        "DELETE FROM contacts WHERE book_id IN (SELECT id FROM contact_books WHERE owner_device_id!=?1) AND NOT EXISTS(SELECT 1 FROM projection_contacts s WHERE s.generation=?2 AND s.book_id=contacts.book_id AND s.contact_id=contacts.id)",
        params![me, generation],
    )?;
    conn.execute(
        "INSERT INTO contacts(id,book_id,revision,body,deleted_at) SELECT s.contact_id,s.book_id,s.revision,s.body,s.deleted_at FROM projection_contacts s JOIN contact_books b ON b.id=s.book_id WHERE s.generation=?2 AND b.owner_device_id!=?1 ON CONFLICT(book_id,id) DO UPDATE SET revision=excluded.revision,body=excluded.body,deleted_at=excluded.deleted_at WHERE excluded.revision>contacts.revision",
        params![me, generation],
    )?;
    // Photo holders follow the stored rows, exactly as after a live apply.
    let mut after = (String::new(), String::new());
    loop {
        let page: Vec<(String, String)> = conn
            .prepare(
                "SELECT book_id,contact_id FROM projection_contacts WHERE generation=? AND (book_id,contact_id)>(?,?) ORDER BY book_id,contact_id LIMIT ?",
            )?
            .query_map(params![generation, after.0, after.1, PROMOTE_PAGE], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        let Some(last) = page.last().cloned() else {
            break;
        };
        for (book_id, contact_id) in &page {
            contact_media::sync_contact(conn, book_id, contact_id)?;
        }
        after = last;
    }
    conn.execute(
        "DELETE FROM contact_photo_references WHERE holder_kind='contact' AND NOT EXISTS(SELECT 1 FROM contacts c WHERE c.book_id=contact_photo_references.book_id AND c.id=contact_photo_references.holder_id)",
        [],
    )?;

    // Notifications from other sources. Tombstones only accumulate (a lifetime never returns).
    conn.execute(
        "DELETE FROM notifications WHERE source_device_id!=?1 AND NOT EXISTS(SELECT 1 FROM projection_notifications s WHERE s.generation=?2 AND s.source_device_id=notifications.source_device_id AND s.notification_key=notifications.notification_key)",
        params![me, generation],
    )?;
    conn.execute(
        "INSERT INTO notification_tombstones(source_device_id,notification_key,lifetime,source_sequence) SELECT source_device_id,notification_key,lifetime,source_sequence FROM projection_tombstones WHERE generation=?2 AND source_device_id!=?1 ON CONFLICT(source_device_id,notification_key,lifetime) DO UPDATE SET source_sequence=MAX(notification_tombstones.source_sequence,excluded.source_sequence)",
        params![me, generation],
    )?;
    conn.execute(
        "INSERT INTO notifications(source_device_id,notification_key,lifetime,instance,package_name,app_name,title,text,category,posted_at,dismissible,seen,removed,source_sequence) SELECT source_device_id,notification_key,lifetime,instance,package_name,app_name,title,text,category,posted_at,dismissible,0,removed,source_sequence FROM projection_notifications WHERE generation=?2 AND source_device_id!=?1 ON CONFLICT(source_device_id,notification_key) DO UPDATE SET seen=CASE WHEN notifications.lifetime=excluded.lifetime THEN notifications.seen ELSE 0 END,lifetime=excluded.lifetime,instance=excluded.instance,package_name=excluded.package_name,app_name=excluded.app_name,title=excluded.title,text=excluded.text,category=excluded.category,posted_at=excluded.posted_at,dismissible=excluded.dismissible,removed=excluded.removed,source_sequence=excluded.source_sequence WHERE excluded.source_sequence>notifications.source_sequence",
        params![me, generation],
    )?;
    conn.execute(
        "UPDATE notification_dismissals SET completed=1 WHERE completed=0 AND EXISTS(SELECT 1 FROM projection_tombstones t WHERE t.generation=? AND t.source_device_id=notification_dismissals.source_device_id AND t.notification_key=notification_dismissals.notification_key AND t.lifetime=notification_dismissals.lifetime)",
        params![generation],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO app_filters(source_device_id,package_name,app_name,muted) SELECT source_device_id,package_name,app_name,0 FROM projection_notifications WHERE generation=?2 AND source_device_id!=?1",
        params![me, generation],
    )?;
    notifications::prune_banners(conn)?;

    // App filters (any source, any writer). This device's own write is kept while there is
    // no proof the snapshot covers it: its outbox row's echo is not journaled at or below the
    // high-water (unsent, or accepted after the snapshot was taken).
    conn.execute(
        "DELETE FROM app_filters WHERE logical_revision>0
           AND NOT EXISTS(SELECT 1 FROM projection_filters s WHERE s.generation=?2 AND s.source_device_id=app_filters.source_device_id AND s.package_name=app_filters.package_name)
           AND NOT (app_filters.producer_device_id=?1 AND EXISTS(SELECT 1 FROM outbox o WHERE o.seq=app_filters.source_sequence
                AND NOT EXISTS(SELECT 1 FROM journal j WHERE j.envelope_id=o.envelope_id AND j.cursor<=?3)))",
        params![me, generation, high_water],
    )?;
    let mut after = (String::new(), String::new());
    loop {
        type Row = (String, String, String, bool, i64, String, i64, String);
        let page: Vec<Row> = conn
            .prepare(
                "SELECT source_device_id,package_name,app_name,muted,source_sequence,producer_device_id,logical_revision,writer_device_id FROM projection_filters WHERE generation=? AND (source_device_id,package_name)>(?,?) ORDER BY source_device_id,package_name LIMIT ?",
            )?
            .query_map(params![generation, after.0, after.1, PROMOTE_PAGE], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        let Some(last) = page.last() else {
            break;
        };
        after = (last.0.clone(), last.1.clone());
        for (source, package, app_name, muted, sequence, producer, revision, writer) in page {
            let u = |v: i64| u64::try_from(v).map_err(|_| Error::Database);
            // Same LWW (and mute side effects) as a live filter event.
            notifications::apply_filter(
                conn,
                &AppFilter {
                    source_device_id: source,
                    package_name: package,
                    app_name,
                    muted,
                },
                u(revision)?,
                &writer,
                &producer,
                u(sequence)?,
            )?;
        }
    }

    conn.execute(
        "UPDATE projection_generations SET state='superseded' WHERE state='promoted' AND generation!=?",
        params![generation],
    )?;
    conn.execute(
        "UPDATE projection_generations SET state='promoted',reason=NULL WHERE generation=?",
        params![generation],
    )?;
    conn.execute("DELETE FROM metadata WHERE k=?", [REPAIR_LATCH])?;
    Ok(())
}

/// Decides, for one pending journal row, whether the newest authoritative snapshot proved it
/// superseded. Loaded once per `apply_pending` batch (after `advance`).
pub(crate) struct Suppression {
    generation: i64,
    high_water: i64,
    draining: bool,
}
impl Suppression {
    pub(crate) fn load(conn: &Connection) -> Result<Option<Self>, Error> {
        Ok(conn
            .query_row(
                "SELECT generation,high_water,state FROM projection_generations WHERE state IN ('draining','staging','promoted') ORDER BY generation DESC LIMIT 1",
                [],
                |r| {
                    Ok(Self {
                        generation: r.get(0)?,
                        high_water: r.get(1)?,
                        draining: r.get::<_, String>(2)? == "draining",
                    })
                },
            )
            .optional()?)
    }

    /// A compactable row at or below the high-water that the snapshot did not retain.
    pub(crate) fn superseded(
        &self,
        conn: &Connection,
        cursor: i64,
        wire: &[u8],
    ) -> Result<bool, Error> {
        if cursor > self.high_water || !compactable(wire) {
            return Ok(false);
        }
        // While draining, undrained snapshot rows are still in `snapshot_records`.
        let retained: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM projection_retained WHERE generation=?1 AND cursor=?2) OR (?3 AND EXISTS(SELECT 1 FROM snapshot_records WHERE generation=?1 AND cursor=?2))",
            params![self.generation, cursor, self.draining],
            |r| r.get(0),
        )?;
        Ok(!retained)
    }
}

/// Bounded removal of stage rows no longer needed (promoted, failed or superseded) and of
/// retained sets that no longer govern suppression.
pub(crate) fn collect_garbage(conn: &Connection) -> Result<(), Error> {
    for table in [
        "projection_books",
        "projection_contacts",
        "projection_notifications",
        "projection_tombstones",
        "projection_filters",
    ] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE rowid IN (SELECT t.rowid FROM {table} t JOIN projection_generations g ON g.generation=t.generation WHERE g.state IN ('promoted','failed','superseded') LIMIT ?)"),
            params![GC_BATCH],
        )?;
    }
    conn.execute(
        "DELETE FROM projection_retained WHERE rowid IN (SELECT r.rowid FROM projection_retained r JOIN projection_generations g ON g.generation=r.generation WHERE g.state IN ('failed','superseded') LIMIT ?)",
        params![GC_BATCH],
    )?;
    conn.execute(
        "DELETE FROM projection_generations WHERE state='superseded' AND NOT EXISTS(SELECT 1 FROM projection_retained r WHERE r.generation=projection_generations.generation)
           AND NOT EXISTS(SELECT 1 FROM projection_books r WHERE r.generation=projection_generations.generation)
           AND NOT EXISTS(SELECT 1 FROM projection_contacts r WHERE r.generation=projection_generations.generation)
           AND NOT EXISTS(SELECT 1 FROM projection_notifications r WHERE r.generation=projection_generations.generation)
           AND NOT EXISTS(SELECT 1 FROM projection_tombstones r WHERE r.generation=projection_generations.generation)
           AND NOT EXISTS(SELECT 1 FROM projection_filters r WHERE r.generation=projection_generations.generation)",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latch(conn: &Connection) -> Option<String> {
        conn.query_row("SELECT v FROM metadata WHERE k=?", [REPAIR_LATCH], |r| {
            r.get(0)
        })
        .optional()
        .unwrap()
    }

    #[test]
    fn upgrade_latches_one_repair_and_fresh_databases_do_not() {
        // Existing database (metadata present, projection tables absent): latched once.
        let upgraded = Connection::open_in_memory().unwrap();
        upgraded
            .execute_batch("CREATE TABLE metadata(k TEXT PRIMARY KEY, v TEXT NOT NULL);")
            .unwrap();
        initialize(&upgraded).unwrap();
        assert_eq!(latch(&upgraded).as_deref(), Some("upgrade"));
        upgraded
            .execute("DELETE FROM metadata WHERE k=?", [REPAIR_LATCH])
            .unwrap();
        // Later opens never re-latch.
        initialize(&upgraded).unwrap();
        assert_eq!(latch(&upgraded), None);

        // Fresh database: initialize runs before the core schema exists.
        let fresh = Connection::open_in_memory().unwrap();
        initialize(&fresh).unwrap();
        fresh
            .execute_batch("CREATE TABLE metadata(k TEXT PRIMARY KEY, v TEXT NOT NULL);")
            .unwrap();
        initialize(&fresh).unwrap();
        assert_eq!(latch(&fresh), None);
    }
}
