//! Owner-local durable contact host state that never syncs: opaque native scan
//! checkpoints and write-once pre-create apply evidence. Both live only in SQLCipher.
use crate::{Ctx, Error, contacts};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
const MAX_EVIDENCE_IDS: usize = 200;
const MAX_ID_BYTES: usize = 1024;

fn invalid() -> Error {
    Error::InvalidRequest("invalid contact host state request")
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn parse(input: &str, allowed: &[&str]) -> Result<Value, Error> {
    let v: Value = serde_json::from_str(input).map_err(|_| invalid())?;
    let obj = v.as_object().ok_or_else(invalid)?;
    if !obj.keys().all(|k| allowed.contains(&k.as_str()))
        || obj
            .get("schema_version")
            .is_some_and(|s| s.as_u64() != Some(1))
    {
        return Err(invalid());
    }
    Ok(v)
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, Error> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= MAX_ID_BYTES)
        .ok_or_else(invalid)
}

pub(crate) fn initialize(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS contact_scan_state(book_id TEXT PRIMARY KEY, checkpoint TEXT NOT NULL, updated_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS contact_apply_evidence(request_id TEXT PRIMARY KEY, source_ids TEXT NOT NULL, recorded_at INTEGER NOT NULL);",
    )?;
    Ok(())
}

/// `{book_id}` reads, `{book_id, checkpoint:{...}}` replaces, `{book_id, clear:true}` deletes
/// the owner's opaque native scan checkpoint (<= 64 KiB encoded).
pub(crate) fn scan_state(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &str,
) -> Result<String, Error> {
    let v = parse(input, &["schema_version", "book_id", "checkpoint", "clear"])?;
    let book_id = text(&v, "book_id")?;
    let clear = match v.get("clear") {
        None => false,
        Some(c) => c.as_bool().ok_or_else(invalid)?,
    };
    let checkpoint = v.get("checkpoint");
    if clear && checkpoint.is_some() {
        return Err(invalid());
    }
    let tx = conn.transaction()?;
    contacts::owned_book(&tx, ctx, book_id)?;
    if clear {
        tx.execute("DELETE FROM contact_scan_state WHERE book_id=?", [book_id])?;
    } else if let Some(checkpoint) = checkpoint {
        if !checkpoint.is_object() {
            return Err(invalid());
        }
        let encoded = serde_json::to_string(checkpoint).map_err(|_| Error::Database)?;
        if encoded.len() > MAX_CHECKPOINT_BYTES {
            return Err(invalid());
        }
        tx.execute(
            "INSERT INTO contact_scan_state(book_id,checkpoint,updated_at) VALUES(?,?,?) ON CONFLICT(book_id) DO UPDATE SET checkpoint=excluded.checkpoint,updated_at=excluded.updated_at",
            params![book_id, encoded, now()],
        )?;
    }
    let row: Option<(String, i64)> = tx
        .query_row(
            "SELECT checkpoint,updated_at FROM contact_scan_state WHERE book_id=?",
            [book_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    tx.commit()?;
    let (checkpoint, updated_at) = match row {
        Some((c, at)) => (
            serde_json::from_str(&c).map_err(|_| Error::Database)?,
            json!(at),
        ),
        None => (Value::Null, Value::Null),
    };
    Ok(json!({"schema_version": 1, "book_id": book_id, "checkpoint": checkpoint, "updated_at": updated_at}).to_string())
}

/// Recorded pre-create evidence for a request, or `Null`.
pub(crate) fn evidence(conn: &Connection, request_id: &str) -> Result<Value, Error> {
    conn.query_row(
        "SELECT source_ids,recorded_at FROM contact_apply_evidence WHERE request_id=?",
        [request_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )
    .optional()?
    .map_or(Ok(Value::Null), |(ids, at)| {
        let ids: Value = serde_json::from_str(&ids).map_err(|_| Error::Database)?;
        Ok(json!({"before_source_ids": ids, "recorded_at": at}))
    })
}

/// Owner-only, write-once evidence for an issued permit: the provider ids that already
/// matched before the OS create, so crash recovery can exclude pre-existing contacts.
/// `{request_id}` reads. Writes are allowed only while the permit is unreconciled; an
/// identical rewrite is idempotent and any different rewrite is rejected.
pub(crate) fn apply_evidence(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &str,
) -> Result<String, Error> {
    let v = parse(
        input,
        &["schema_version", "request_id", "before_source_ids"],
    )?;
    let id = text(&v, "request_id")?;
    let tx = conn.transaction()?;
    let (book_id, status, issued, body): (String, String, bool, String) = tx
        .query_row(
            "SELECT book_id,status,permit_issued,body FROM contact_edit_ledger WHERE request_id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    contacts::owned_book(&tx, ctx, &book_id)?;
    let request: Value = serde_json::from_str(&body).map_err(|_| Error::Database)?;
    if !issued || text(&request, "target_owner")? != ctx.device_id.to_string() {
        return Err(Error::InvalidRequest("no issued contact apply permit"));
    }
    if let Some(ids) = v.get("before_source_ids") {
        let ids = ids
            .as_array()
            .filter(|ids| ids.len() <= MAX_EVIDENCE_IDS)
            .ok_or_else(invalid)?;
        let mut clean = ids
            .iter()
            .map(|id| {
                id.as_str()
                    .filter(|s| !s.is_empty() && s.len() <= MAX_ID_BYTES)
                    .map(str::to_owned)
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(invalid)?;
        clean.sort();
        clean.dedup();
        let encoded = serde_json::to_string(&clean).map_err(|_| Error::Database)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT source_ids FROM contact_apply_evidence WHERE request_id=?",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(existing) if existing == encoded => {}
            Some(_) => return Err(Error::InvalidRequest("contact apply evidence is immutable")),
            None if matches!(status.as_str(), "applying" | "outcome_unknown") => {
                tx.execute(
                    "INSERT INTO contact_apply_evidence(request_id,source_ids,recorded_at) VALUES(?,?,?)",
                    params![id, encoded, now()],
                )?;
            }
            None => return Err(Error::InvalidRequest("contact apply already reconciled")),
        }
    }
    let out = evidence(&tx, id)?;
    tx.commit()?;
    Ok(
        json!({"schema_version": 1, "request_id": id, "state": status, "evidence": out})
            .to_string(),
    )
}
