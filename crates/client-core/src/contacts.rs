//! Encrypted contact projection and the owner-side edit ledger.
//!
//! Native providers supply contact content plus opaque provider identifiers. Core strips
//! every provider identifier into local-only tables before anything is projected or
//! enqueued, and core alone mints contact revisions (from a content digest) so peers can
//! gate replays. Edit requests are authorized only by the owner device: the remote edit
//! policy, conflict checks and deletion caps are enforced here, never in native code.
use crate::{Ctx, Error, PrivatePayload, enqueue};
use peppy_protocol::EnvelopePurpose;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_CONTACTS: usize = 20_000;
const MAX_VALUES: usize = 20;
const MAX_TEXT: usize = 1024;
const MAX_NOTES: usize = 8192;
const MAX_PATCHES: usize = 100;
const TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
/// Tolerated clock skew for peer-supplied expiries.
const EXPIRY_SKEW_SECONDS: i64 = 5 * 60;
const ROLLING_WINDOW_SECONDS: i64 = 24 * 60 * 60;
/// Mode enforced when a book carries no explicit policy (user-approved default; deletes
/// remain subject to the rolling cap).
pub(crate) const DEFAULT_REMOTE_EDITS: &str = "auto";
/// Unchanged book state is republished at most once per day.
const BOOK_HEARTBEAT_SECONDS: i64 = 24 * 60 * 60;
/// Absolute and percentage thresholds shared by the scan and remote-delete policies.
const DELETE_LIMIT_ABS: i64 = 10;
const DELETE_LIMIT_PCT: i64 = 5;
/// Keys that carry native provider identity and never leave the owner device.
const SOURCE_KEYS: [&str; 4] = [
    "provenance",
    "source_id",
    "account_id",
    "default_account_id",
];
const SCALAR_FIELDS: [&str; 5] = ["display_name", "nickname", "organization", "title", "notes"];
const NAME_PARTS: [&str; 5] = ["given", "family", "middle", "prefix", "suffix"];
const LIST_FIELDS: [&str; 3] = ["phones", "emails", "addresses"];
/// Structured postal address keys shared with the iOS/Android adapters and desktop UI.
const ADDRESS_KEYS: [&str; 14] = [
    "id",
    "label",
    "value",
    "street",
    "po_box",
    "neighborhood",
    "sub_locality",
    "city",
    "sub_administrative_area",
    "state",
    "postal_code",
    "country",
    "iso_country_code",
    "writable",
];
const CREATE_FIELDS: [&str; 11] = [
    "birthday",
    "display_name",
    "name",
    "nickname",
    "organization",
    "title",
    "phones",
    "emails",
    "addresses",
    "notes",
    "photo",
];
const REQUEST_KEYS: [&str; 12] = [
    "schema_version",
    "request_id",
    "target_owner",
    "book_id",
    "kind",
    "contact_id",
    "base_revision",
    "expected_old",
    "patches",
    "photo_op",
    "expires_at",
    "provenance",
];
const TERMINAL: [&str; 5] = ["applied", "conflict", "rejected", "expired", "failed"];
const RESULT_STATUSES: [&str; 7] = [
    "awaiting_approval",
    "applied",
    "conflict",
    "rejected",
    "expired",
    "failed",
    "outcome_unknown",
];

#[derive(Clone, Debug, Deserialize)]
struct ContactFields {
    display_name: String,
    #[serde(default)]
    name: Value,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    organization: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    phones: Vec<Value>,
    #[serde(default)]
    emails: Vec<Value>,
    #[serde(default)]
    addresses: Vec<Value>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    birthday: Value,
}

fn invalid() -> Error {
    Error::InvalidRequest("invalid contact request")
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub(crate) fn req<'a>(v: &'a Value, key: &str) -> Result<&'a str, Error> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= MAX_TEXT)
        .ok_or_else(invalid)
}
/// Canonical non-negative decimal string (no sign, no leading zeros).
pub(crate) fn revision(v: &Value, key: &str) -> Result<i64, Error> {
    let text = req(v, key)?;
    if (text != "0" && text.starts_with('0')) || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    text.parse::<i64>().map_err(|_| invalid())
}
fn encoded(v: &Value) -> Result<String, Error> {
    serde_json::to_string(v).map_err(|_| Error::Database)
}
fn parse(input: &str) -> Result<Value, Error> {
    serde_json::from_str(input).map_err(|_| invalid())
}
fn schema_v1(v: &Value) -> Result<(), Error> {
    if v.get("schema_version").and_then(Value::as_u64) == Some(1) {
        Ok(())
    } else {
        Err(invalid())
    }
}
pub(crate) fn require_unlocked(ctx: &Ctx<'_>) -> Result<(), Error> {
    match ctx.active_epoch {
        Some(epoch) if ctx.keys.contains_key(&epoch) => Ok(()),
        _ => Err(Error::KeysUnavailable),
    }
}
fn short_text(v: Option<&Value>, max: usize) -> bool {
    v.is_none_or(|v| v.is_null() || v.as_str().is_some_and(|s| s.chars().count() <= max))
}

/// Deterministic JSON text with sorted object keys, independent of serde_json features.
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}
fn digest(body: &Value) -> String {
    let mut text = String::new();
    canonical(body, &mut text);
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Recursively removes provider identity; account entries lose their opaque `id`.
pub(crate) fn strip_sources(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for key in SOURCE_KEYS {
                map.remove(key);
            }
            if let Some(Value::Array(accounts)) = map.get_mut("accounts") {
                for account in accounts.iter_mut() {
                    if let Some(account) = account.as_object_mut() {
                        account.remove("id");
                    }
                }
            }
            for child in map.values_mut() {
                strip_sources(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_sources),
        _ => {}
    }
}

/// Address items carry only the explicit structured keys, each bounded text (or the
/// `writable` flag).
fn valid_address(item: &Value) -> bool {
    req(item, "id").is_ok()
        && item.as_object().is_some_and(|obj| {
            obj.iter().all(|(k, v)| match (k.as_str(), v) {
                ("writable", Value::Bool(_)) => true,
                (k, Value::String(s)) => ADDRESS_KEYS.contains(&k) && s.chars().count() <= MAX_TEXT,
                (k, Value::Null) => ADDRESS_KEYS.contains(&k),
                _ => false,
            })
        })
}

fn valid_list_item(list: &str, item: &Value) -> bool {
    if list == "addresses" {
        valid_address(item)
    } else {
        valid_item(item)
    }
}

/// Gregorian birthday `{year?, month, day}`; a yearless Feb 29 is valid.
fn valid_birthday(v: &Value) -> bool {
    let Some(obj) = v.as_object() else {
        return false;
    };
    if !obj
        .keys()
        .all(|k| matches!(k.as_str(), "year" | "month" | "day"))
    {
        return false;
    }
    let year = match obj.get("year") {
        None | Some(Value::Null) => None,
        Some(y) => match y.as_i64().filter(|y| (1..=9999).contains(y)) {
            Some(y) => Some(y),
            None => return false,
        },
    };
    let (Some(month), Some(day)) = (
        obj.get("month").and_then(Value::as_i64),
        obj.get("day").and_then(Value::as_i64),
    ) else {
        return false;
    };
    let leap = year.is_none_or(|y| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

fn valid_item(item: &Value) -> bool {
    req(item, "id").is_ok()
        && item.as_object().is_some_and(|obj| {
            obj.values().all(|v| match v {
                Value::String(s) => s.chars().count() <= MAX_TEXT,
                Value::Bool(_) | Value::Number(_) | Value::Null => true,
                _ => false,
            })
        })
}

/// Validates contact content (not identity or revision).
fn validate_fields(v: &Value) -> Result<(), Error> {
    let fields: ContactFields = serde_json::from_value(v.clone()).map_err(|_| invalid())?;
    let names_ok = match &fields.name {
        Value::Null => true,
        // Providers may carry extra name parts (e.g. phonetic); patches stay restricted.
        Value::Object(map) => {
            map.len() <= MAX_VALUES && map.values().all(|v| short_text(Some(v), MAX_TEXT))
        }
        _ => false,
    };
    let lists = [&fields.phones, &fields.emails, &fields.addresses];
    if fields.display_name.chars().count() > MAX_TEXT
        || !names_ok
        || [&fields.nickname, &fields.organization, &fields.title]
            .into_iter()
            .any(|s| s.as_ref().is_some_and(|s| s.chars().count() > MAX_TEXT))
        || fields.notes.as_ref().is_some_and(|s| s.len() > MAX_NOTES)
        || lists.iter().any(|items| items.len() > MAX_VALUES)
        || !fields.phones.iter().chain(&fields.emails).all(valid_item)
        || !fields.addresses.iter().all(valid_address)
        || !(fields.birthday.is_null() || valid_birthday(&fields.birthday))
    {
        return Err(invalid());
    }
    Ok(())
}

/// Native contact input -> (id, sanitized body with core digest, local-only provenance).
/// Any native-supplied `revision`/`digest` is ignored: core mints both.
fn native_contact(v: &Value, book_id: &str) -> Result<(String, Value, Option<Value>), Error> {
    let id = req(v, "id")?.to_owned();
    if req(v, "book_id")? != book_id {
        return Err(invalid());
    }
    validate_fields(v)?;
    let mut body = v.clone();
    let obj = body.as_object_mut().ok_or_else(invalid)?;
    let provenance = obj.remove("provenance");
    for key in ["revision", "digest", "generation", "deleted_at"] {
        obj.remove(key);
    }
    strip_sources(&mut body);
    let hash = digest(&body);
    body["digest"] = Value::String(hash);
    Ok((id, body, provenance))
}

/// Peer contact payload: identity + canonical core revision + valid content.
pub(crate) fn peer_contact(v: &Value, book_id: &str) -> Result<(String, i64, Value), Error> {
    let id = req(v, "id")?.to_owned();
    if req(v, "book_id")? != book_id {
        return Err(invalid());
    }
    let rev = revision(v, "revision")?;
    validate_fields(v)?;
    let mut body = v.clone();
    strip_sources(&mut body);
    Ok((id, rev, body))
}

pub(crate) fn initialize(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        "\
CREATE TABLE IF NOT EXISTS contact_books(id TEXT PRIMARY KEY, owner_device_id TEXT NOT NULL, generation INTEGER NOT NULL, state TEXT NOT NULL, body TEXT NOT NULL, forgotten INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS contacts(id TEXT NOT NULL, book_id TEXT NOT NULL REFERENCES contact_books(id) ON DELETE CASCADE, revision INTEGER NOT NULL, body TEXT NOT NULL, deleted_at INTEGER, PRIMARY KEY(book_id,id));
CREATE TABLE IF NOT EXISTS contact_scans(scan_id TEXT PRIMARY KEY, book_id TEXT NOT NULL, generation INTEGER NOT NULL, access TEXT NOT NULL, authoritative INTEGER NOT NULL, complete INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS contact_scan_members(scan_id TEXT NOT NULL, contact_id TEXT NOT NULL, PRIMARY KEY(scan_id,contact_id));
CREATE TABLE IF NOT EXISTS contact_edit_ledger(request_id TEXT PRIMARY KEY, book_id TEXT NOT NULL, requester TEXT NOT NULL, body TEXT NOT NULL, status TEXT NOT NULL, expires_at INTEGER NOT NULL, permit_issued INTEGER NOT NULL DEFAULT 0, observed TEXT, key_epoch INTEGER);
CREATE TABLE IF NOT EXISTS contact_book_sources(book_id TEXT PRIMARY KEY, body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS contact_scan_holds(scan_id TEXT PRIMARY KEY, book_id TEXT NOT NULL, generation INTEGER NOT NULL, members TEXT NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS contact_scan_baselines(scan_id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS contact_scan_baseline(scan_id TEXT NOT NULL, contact_id TEXT NOT NULL, revision INTEGER NOT NULL, PRIMARY KEY(scan_id,contact_id));
CREATE TABLE IF NOT EXISTS contact_restore_claims(book_id TEXT NOT NULL, tombstone_id TEXT NOT NULL, request_id TEXT NOT NULL, status TEXT NOT NULL, contact_id TEXT NOT NULL, PRIMARY KEY(book_id,tombstone_id));
",
    )?;
    let has_key_epoch = conn
        .prepare("PRAGMA table_info(contact_edit_ledger)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == "key_epoch");
    if !has_key_epoch {
        conn.execute(
            "ALTER TABLE contact_edit_ledger ADD COLUMN key_epoch INTEGER",
            [],
        )?;
    }
    Ok(())
}

/// Owned, non-forgotten book (sanitized body, generation). Non-owners are rejected.
pub(crate) fn owned_book(
    conn: &Connection,
    ctx: &Ctx<'_>,
    book_id: &str,
) -> Result<(Value, i64), Error> {
    let (owner, generation, body): (String, i64, String) = conn
        .query_row(
            "SELECT owner_device_id,generation,body FROM contact_books WHERE id=? AND forgotten=0",
            [book_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    if owner != ctx.device_id.to_string() {
        return Err(Error::InvalidRequest("not contact book owner"));
    }
    let body = serde_json::from_str(&body).map_err(|_| Error::Database)?;
    Ok((body, generation))
}

/// Owner-minted book-state revision (`state_revision`, canonical string; absent = 0).
pub(crate) fn book_state_revision(book: &Value) -> Result<i64, Error> {
    match book.get("state_revision") {
        None => Ok(0),
        Some(_) => revision(book, "state_revision"),
    }
}

/// Whether `incoming` (at `generation`) replaces the stored book state. A higher
/// generation always wins; within a generation `retired` is terminal and otherwise only a
/// strictly newer owner `state_revision` applies, so a delayed or replayed record that
/// embeds an older book (e.g. a photo-held upsert) never rewinds state/capability/policy.
pub(crate) fn book_supersedes(
    stored_generation: i64,
    stored: &Value,
    generation: i64,
    incoming: &Value,
) -> Result<bool, Error> {
    let incoming_revision = book_state_revision(incoming)?;
    if generation != stored_generation {
        return Ok(generation > stored_generation);
    }
    if stored.get("state").and_then(Value::as_str) == Some("retired") {
        return Ok(false);
    }
    Ok(incoming_revision > book_state_revision(stored).unwrap_or(0))
}

/// Sanitized stored book body (no ownership check).
fn book_body(conn: &Connection, book_id: &str) -> Result<Value, Error> {
    let body: String = conn.query_row(
        "SELECT body FROM contact_books WHERE id=?",
        [book_id],
        |r| r.get(0),
    )?;
    serde_json::from_str(&body).map_err(|_| Error::Database)
}

/// Owner-side upsert: mints `revision = previous + 1` only when the digest changed (or the
/// row is new/deleted) and publishes the sanitized contact. Returns the stored body.
fn upsert_owned(
    conn: &Connection,
    ctx: &Ctx<'_>,
    book: &Value,
    book_id: &str,
    id: &str,
    mut body: Value,
    provenance: Option<Value>,
) -> Result<(Value, bool), Error> {
    crate::contact_source::record_source(conn, book_id, id, provenance)?;
    let existing: Option<(i64, String, Option<i64>)> = conn
        .query_row(
            "SELECT revision,body,deleted_at FROM contacts WHERE book_id=? AND id=?",
            params![book_id, id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((_, old, None)) = &existing {
        let old: Value = serde_json::from_str(old).map_err(|_| Error::Database)?;
        if old.get("digest") == body.get("digest") {
            return Ok((old, false));
        }
    }
    let rev = existing.map_or(Ok(1), |(rev, _, _)| {
        rev.checked_add(1).ok_or(Error::Database)
    })?;
    body["revision"] = Value::String(rev.to_string());
    conn.execute(
        "INSERT INTO contacts(id,book_id,revision,body,deleted_at) VALUES(?,?,?,?,NULL) ON CONFLICT(book_id,id) DO UPDATE SET revision=excluded.revision,body=excluded.body,deleted_at=NULL",
        params![id, book_id, rev, encoded(&body)?],
    )?;
    enqueue(
        conn,
        ctx,
        EnvelopePurpose::Event,
        None,
        None,
        &PrivatePayload::ContactUpserted {
            book: book.clone(),
            contact: body.clone(),
        },
    )?;
    Ok((body, true))
}

/// Owner-side removal: bumps the revision so peers can gate stale upserts and tombstones.
fn tombstone_owned(
    conn: &Connection,
    ctx: &Ctx<'_>,
    book_id: &str,
    id: &str,
    deleted_at: i64,
) -> Result<Option<Value>, Error> {
    let Some((rev, body)): Option<(i64, String)> = conn
        .query_row(
            "SELECT revision,body FROM contacts WHERE book_id=? AND id=? AND deleted_at IS NULL",
            params![book_id, id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let rev = rev.checked_add(1).ok_or(Error::Database)?;
    let mut body: Value = serde_json::from_str(&body).map_err(|_| Error::Database)?;
    body["revision"] = Value::String(rev.to_string());
    conn.execute(
        "UPDATE contacts SET revision=?,body=?,deleted_at=? WHERE book_id=? AND id=?",
        params![rev, encoded(&body)?, deleted_at, book_id, id],
    )?;
    // Self-contained: the book lets a fresh receiver (compacted [removal, book] order)
    // keep the 90-day restore data; receivers gate it like any book state.
    let tombstone = json!({
        "contact_id": id,
        "revision": rev.to_string(),
        "deleted_at": deleted_at,
        "restored_contact": body,
        "book": book_body(conn, book_id)?,
    });
    enqueue(
        conn,
        ctx,
        EnvelopePurpose::Event,
        None,
        None,
        &PrivatePayload::ContactRemoved {
            book_id: book_id.to_owned(),
            contact_id: id.to_owned(),
            tombstone: tombstone.clone(),
        },
    )?;
    Ok(Some(tombstone))
}

/// Links a deleted contact to the new identity that restored it and republishes the
/// tombstone (revision bumped) so every device hides it from restore lists.
fn mark_restored(
    conn: &Connection,
    ctx: &Ctx<'_>,
    book_id: &str,
    old_id: &str,
    new_id: &str,
) -> Result<(), Error> {
    let Some((rev, body, deleted_at)): Option<(i64, String, i64)> = conn
        .query_row(
            "SELECT revision,body,deleted_at FROM contacts WHERE book_id=? AND id=? AND deleted_at IS NOT NULL",
            params![book_id, old_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
    else {
        return Ok(());
    };
    let rev = rev.checked_add(1).ok_or(Error::Database)?;
    let mut body: Value = serde_json::from_str(&body).map_err(|_| Error::Database)?;
    body["revision"] = Value::String(rev.to_string());
    body["restored_to"] = json!(new_id);
    conn.execute(
        "UPDATE contacts SET revision=?,body=? WHERE book_id=? AND id=?",
        params![rev, encoded(&body)?, book_id, old_id],
    )?;
    enqueue(
        conn,
        ctx,
        EnvelopePurpose::Event,
        None,
        None,
        &PrivatePayload::ContactRemoved {
            book_id: book_id.to_owned(),
            contact_id: old_id.to_owned(),
            tombstone: json!({
                "contact_id": old_id,
                "revision": rev.to_string(),
                "deleted_at": deleted_at,
                "restored_contact": body,
                "book": book_body(conn, book_id)?,
            }),
        },
    )?;
    Ok(())
}

/// Capture is atomic with its encrypted state/upsert events.  A locked vault has
/// no contact plaintext backlog, so providers must retry after unlock.
pub(crate) fn capture(conn: &mut Connection, ctx: &Ctx<'_>, input: &str) -> Result<String, Error> {
    require_unlocked(ctx)?;
    let input = parse(input)?;
    schema_v1(&input)?;
    let book = input.get("book").ok_or_else(invalid)?;
    let contacts = input
        .get("contacts")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let tx = conn.transaction()?;
    let captured = capture_tx(&tx, ctx, book, contacts)?;
    tx.commit()?;
    let changed = captured.iter().filter(|(_, _, changed)| *changed).count();
    Ok(
        json!({"status":"captured","book_id":req(book, "id")?,"count":contacts.len(),"changed":changed})
            .to_string(),
    )
}

/// Keeps a core-stored default account (owner settings) while it is still one of the
/// captured accounts; native's value is used only when core has none.
fn preserve_default_account(tx: &Connection, book_id: &str, book: &Value) -> Result<Value, Error> {
    let mut raw = book.clone();
    let stored = book_source(tx, book_id)?;
    if let Some(default) = stored.get("default_account_id").filter(|d| d.is_string()) {
        let listed = book
            .get("accounts")
            .and_then(Value::as_array)
            .is_none_or(|accounts| {
                accounts.iter().any(|a| {
                    a.get("id") == Some(default)
                        && a.get("writable").and_then(Value::as_bool) != Some(false)
                })
            });
        if listed {
            raw["default_account_id"] = default.clone();
        }
    }
    Ok(raw)
}

/// Book state plus contacts inside the caller's transaction; shared by the legacy DTO and
/// the platform-source capture. Returns `(contact id, core revision, changed)` per contact.
pub(crate) fn capture_tx(
    tx: &Connection,
    ctx: &Ctx<'_>,
    book: &Value,
    contacts: &[Value],
) -> Result<Vec<(String, Value, bool)>, Error> {
    require_unlocked(ctx)?;
    let book_id = req(book, "id")?;
    if req(book, "owner_device_id")? != ctx.device_id.to_string() {
        return Err(invalid());
    }
    let generation = revision(book, "generation")?;
    if let Some(mode) = book.pointer("/policy/remote_edits")
        && !matches!(mode.as_str(), Some("auto" | "confirm" | "off"))
    {
        return Err(invalid());
    }
    if contacts.len() > MAX_CONTACTS {
        return Err(invalid());
    }
    let stored: Option<(String, i64, String)> = tx
        .query_row(
            "SELECT owner_device_id,generation,body FROM contact_books WHERE id=?",
            [book_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let mut public = book.clone();
    strip_sources(&mut public);
    // `inventory` is core-computed; native input never sets it.
    public
        .as_object_mut()
        .ok_or_else(invalid)?
        .remove("inventory");
    let obj = public.as_object_mut().ok_or_else(invalid)?;
    obj.remove("state_revision");
    obj.insert("state_revision".into(), json!("0"));
    if let Some((owner, stored_generation, stored_body)) = stored {
        if owner != ctx.device_id.to_string() || generation < stored_generation {
            return Err(invalid());
        }
        // Owner settings (contact_settings_json) are authoritative over captured policy;
        // otherwise a capture that omits policy keeps the stored one.
        let stored_body: Value = serde_json::from_str(&stored_body).map_err(|_| Error::Database)?;
        if let Some(inventory) = stored_body.get("inventory") {
            public["inventory"] = inventory.clone();
        }
        // Core mints the book-state revision; hosts never supply it.
        public["state_revision"] = stored_body
            .get("state_revision")
            .cloned()
            .unwrap_or_else(|| json!("0"));
        // Retirement is terminal within a generation; reactivation needs a new one.
        if generation == stored_generation
            && stored_body.get("state").and_then(Value::as_str) == Some("retired")
        {
            public["state"] = json!("retired");
        }
        if stored_body.get("last_config_at").is_some() || public.get("policy").is_none() {
            for key in ["policy", "last_config_at"] {
                if let Some(value) = stored_body.get(key) {
                    public[key] = value.clone();
                }
            }
        }
    }
    // The large-delete approval is mandatory; the flag is informational and always true.
    if let Some(policy) = public.get_mut("policy").and_then(Value::as_object_mut) {
        policy.insert("large_delete_requires_approval".into(), json!(true));
    }
    let raw_book = preserve_default_account(tx, book_id, book)?;
    tx.execute(
        "INSERT INTO contact_books(id,owner_device_id,generation,state,body,forgotten) VALUES(?,?,?,?,?,0) ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,state=excluded.state,body=excluded.body,forgotten=0",
        params![
            book_id,
            ctx.device_id.to_string(),
            generation,
            public.get("state").and_then(Value::as_str).unwrap_or("active"),
            encoded(&public)?
        ],
    )?;
    tx.execute(
        "INSERT INTO contact_book_sources(book_id,body) VALUES(?,?) ON CONFLICT(book_id) DO UPDATE SET body=excluded.body",
        params![book_id, encoded(&raw_book)?],
    )?;
    let mut out = Vec::with_capacity(contacts.len());
    for contact in contacts {
        let (id, body, provenance) = native_contact(contact, book_id)?;
        let (stored, changed) = upsert_owned(tx, ctx, &public, book_id, &id, body, provenance)?;
        out.push((id, stored["revision"].clone(), changed));
    }
    let alive: i64 = tx.query_row(
        "SELECT COUNT(*) FROM contacts WHERE book_id=? AND deleted_at IS NULL",
        [book_id],
        |r| r.get(0),
    )?;
    if alive > MAX_CONTACTS as i64 {
        return Err(invalid());
    }
    publish_book_state(tx, ctx, book_id)?;
    Ok(out)
}

/// Recomputes the core inventory marker (`count` and a digest over sorted
/// `(contact id, revision)` of live contacts) and publishes the sanitized book state only
/// when it differs from the last published state, or as a 24h heartbeat. Identical
/// rescans therefore enqueue nothing. The marker is a drift signal, not an inventory.
pub(crate) fn publish_book_state(
    tx: &Connection,
    ctx: &Ctx<'_>,
    book_id: &str,
) -> Result<bool, Error> {
    let (mut body, _) = owned_book(tx, ctx, book_id)?;
    let live = tx
        .prepare(
            "SELECT id,revision FROM contacts WHERE book_id=? AND deleted_at IS NULL ORDER BY id",
        )?
        .query_map([book_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut hasher = Sha256::new();
    for (id, rev) in &live {
        hasher.update(id.as_bytes());
        hasher.update([0]);
        hasher.update(rev.to_string().as_bytes());
        hasher.update(b"\n");
    }
    let inventory: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    body["inventory"] = json!({"count": live.len(), "digest": inventory});
    let mut content = body.clone();
    content
        .as_object_mut()
        .ok_or(Error::Database)?
        .remove("state_revision");
    let state = digest(&content);
    let key = format!("contact_book_published:{book_id}");
    let at = now();
    let (changed, heartbeat) = match crate::get_meta(tx, &key)? {
        Some(last) => {
            let (ts, published) = last.split_once(':').unwrap_or(("0", ""));
            (
                published != state,
                at - ts.parse::<i64>().unwrap_or(0) >= BOOK_HEARTBEAT_SECONDS,
            )
        }
        None => (true, false),
    };
    if changed {
        let next = book_state_revision(&body)?
            .checked_add(1)
            .ok_or(Error::Database)?;
        body["state_revision"] = json!(next.to_string());
    }
    tx.execute(
        "UPDATE contact_books SET body=? WHERE id=?",
        params![encoded(&body)?, book_id],
    )?;
    if !changed && !heartbeat {
        return Ok(false);
    }
    enqueue(
        tx,
        ctx,
        EnvelopePurpose::Event,
        None,
        None,
        &PrivatePayload::ContactBookState { book: body },
    )?;
    crate::set_meta(tx, &key, &format!("{at}:{state}"))?;
    Ok(true)
}

pub(crate) fn begin_scan(conn: &Connection, ctx: &Ctx<'_>, input: &str) -> Result<String, Error> {
    let v = parse(input)?;
    schema_v1(&v)?;
    let scan_id = req(&v, "scan_id")?;
    let book_id = req(&v, "book_id")?;
    let generation = revision(&v, "generation")?;
    let access = req(&v, "access")?;
    if !matches!(access, "full" | "limited" | "unavailable") {
        return Err(invalid());
    }
    let authoritative = v
        .get("authoritative")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    owned_book(conn, ctx, book_id)?;
    let existing: Option<(String, i64, String, bool, bool)> = conn
        .query_row(
            "SELECT book_id,generation,access,authoritative,complete FROM contact_scans WHERE scan_id=?",
            [scan_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    if let Some((b, g, a, auth, finished)) = existing {
        if b != book_id || g != generation || a != access || auth != authoritative {
            return Err(invalid());
        }
        let status = if finished { "finished" } else { "started" };
        return Ok(json!({"status":status,"scan_id":scan_id}).to_string());
    }
    // Fence: only contacts live at begin, still at their begin revision, may be inferred
    // missing; rows created or rewritten during a multi-pass scan are never removed by it.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO contact_scans(scan_id,book_id,generation,access,authoritative,complete) VALUES(?,?,?,?,?,0)",
        params![scan_id, book_id, generation, access, authoritative],
    )?;
    tx.execute(
        "INSERT INTO contact_scan_baselines(scan_id) VALUES(?)",
        [scan_id],
    )?;
    tx.execute(
        "INSERT INTO contact_scan_baseline(scan_id,contact_id,revision) SELECT ?1,id,revision FROM contacts WHERE book_id=?2 AND deleted_at IS NULL",
        params![scan_id, book_id],
    )?;
    tx.commit()?;
    Ok(json!({"status":"started","scan_id":scan_id}).to_string())
}

/// Observing inserts new contacts and mints a new revision for changed ones.
pub(crate) fn observe_scan(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &str,
) -> Result<String, Error> {
    require_unlocked(ctx)?;
    let v = parse(input)?;
    let scan = req(&v, "scan_id")?;
    let tx = conn.transaction()?;
    let (book_id, finished): (String, bool) = tx
        .query_row(
            "SELECT book_id,complete FROM contact_scans WHERE scan_id=?",
            [scan],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    if finished {
        return Err(Error::InvalidRequest("contact scan already finished"));
    }
    let (book, _) = owned_book(&tx, ctx, &book_id)?;
    // Either the legacy contact DTO or a platform `source` entry mapped by core.
    let contact = observed_contact(&tx, &v, "contact", "source", &book_id)?;
    let (id, body, provenance) = native_contact(&contact, &book_id)?;
    tx.execute(
        "INSERT OR IGNORE INTO contact_scan_members(scan_id,contact_id) VALUES(?,?)",
        params![scan, id],
    )?;
    let (stored, changed) = upsert_owned(&tx, ctx, &book, &book_id, &id, body, provenance)?;
    tx.commit()?;
    Ok(json!({"status":"observed","contact_id":id,"changed":changed,"revision":stored["revision"]}).to_string())
}

/// Any finish is terminal. Only a complete, full, authoritative scan at the current
/// generation infers removals, and only up to `max(10, 5%)` per scan; more is held.
pub(crate) fn finish_scan(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &str,
) -> Result<String, Error> {
    require_unlocked(ctx)?;
    let v = parse(input)?;
    let scan = req(&v, "scan_id")?;
    let complete = v.get("complete").and_then(Value::as_bool).unwrap_or(false);
    let tx = conn.transaction()?;
    let (book_id, generation, access, authoritative, finished): (String, i64, String, bool, bool) =
        tx.query_row(
            "SELECT book_id,generation,access,authoritative,complete FROM contact_scans WHERE scan_id=?",
            [scan],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    if finished {
        return Ok(json!({"status":"finished","already_finished":true,"deletions_applied":false,"deleted":0,"held":0}).to_string());
    }
    let (_, current_generation) = owned_book(&tx, ctx, &book_id)?;
    let mut hold_id = None;
    tx.execute(
        "UPDATE contact_scans SET complete=1 WHERE scan_id=?",
        [scan],
    )?;
    let safe = complete && access == "full" && authoritative && current_generation == generation;
    let (mut deleted, mut held) = (0usize, 0usize);
    if safe {
        // A newer complete authoritative scan supersedes every earlier pending hold.
        tx.execute(
            "UPDATE contact_scan_holds SET status='stale' WHERE book_id=? AND status='awaiting_approval' AND scan_id!=?",
            params![book_id, scan],
        )?;
        let fenced: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM contact_scan_baselines WHERE scan_id=?)",
            [scan],
            |r| r.get(0),
        )?;
        let query = if fenced {
            "SELECT c.id FROM contacts c JOIN contact_scan_baseline b ON b.scan_id=?2 AND b.contact_id=c.id AND b.revision=c.revision WHERE c.book_id=?1 AND c.deleted_at IS NULL AND c.id NOT IN (SELECT contact_id FROM contact_scan_members WHERE scan_id=?2) ORDER BY c.id"
        } else {
            "SELECT id FROM contacts WHERE book_id=?1 AND deleted_at IS NULL AND id NOT IN (SELECT contact_id FROM contact_scan_members WHERE scan_id=?2) ORDER BY id"
        };
        let missing = tx
            .prepare(query)?
            .query_map(params![book_id, scan], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM contacts WHERE book_id=? AND deleted_at IS NULL",
            [&book_id],
            |r| r.get(0),
        )?;
        let limit = DELETE_LIMIT_ABS.max(total * DELETE_LIMIT_PCT / 100);
        if missing.len() as i64 > limit {
            // Durable hold: approval later tombstones exactly these contacts at these
            // revisions, once, and only while the book generation is unchanged.
            let mut members = Vec::with_capacity(missing.len());
            for id in &missing {
                let rev: i64 = tx.query_row(
                    "SELECT revision FROM contacts WHERE book_id=? AND id=?",
                    params![book_id, id],
                    |r| r.get(0),
                )?;
                members.push(json!({"id": id, "revision": rev.to_string()}));
            }
            tx.execute(
                "INSERT INTO contact_scan_holds(scan_id,book_id,generation,members,status,created_at) VALUES(?,?,?,?,'awaiting_approval',?)",
                params![scan, book_id, generation, encoded(&Value::Array(members))?, now()],
            )?;
            held = missing.len();
            hold_id = Some(scan.to_owned());
        } else {
            let at = now();
            for id in &missing {
                if tombstone_owned(&tx, ctx, &book_id, id, at)?.is_some() {
                    deleted += 1;
                }
            }
        }
    }
    tx.execute("DELETE FROM contact_scan_baseline WHERE scan_id=?", [scan])?;
    publish_book_state(&tx, ctx, &book_id)?;
    tx.commit()?;
    let mut out =
        json!({"status":"finished","deletions_applied":deleted > 0,"deleted":deleted,"held":held});
    if let Some(hold_id) = hold_id {
        out["state"] = json!("awaiting_approval");
        out["hold_id"] = json!(hold_id);
    }
    Ok(out.to_string())
}

pub(crate) fn view(conn: &Connection, input: &str) -> Result<String, Error> {
    let v = parse(input)?;
    let book_id = req(&v, "book_id")?;
    let limit = v
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .min(200) as usize;
    let offset = v.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let search = v
        .get("search")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let book: Value = serde_json::from_str(
        &conn
            .query_row(
                "SELECT body FROM contact_books WHERE id=? AND forgotten=0",
                [book_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(Error::NotFound)?,
    )
    .map_err(|_| Error::Database)?;
    // Search runs before pagination and only over displayable text.
    let contacts = conn
        .prepare("SELECT body FROM contacts WHERE book_id=? AND deleted_at IS NULL ORDER BY id")?
        .query_map([book_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|c| search.is_empty() || searchable(c).contains(&search))
        .skip(offset)
        .take(limit)
        .map(|c| crate::contact_media::present(conn, sanitize(c)))
        .collect::<Vec<_>>();
    Ok(json!({"schema_version":1,"book":sanitize(book),"contacts":contacts,"offset":offset,"limit":limit}).to_string())
}

fn searchable(contact: &Value) -> String {
    let mut text = String::new();
    for key in ["display_name", "nickname", "organization", "title"] {
        if let Some(s) = contact.get(key).and_then(Value::as_str) {
            text.push_str(s);
            text.push('\n');
        }
    }
    if let Some(name) = contact.get("name").and_then(Value::as_object) {
        name.values().filter_map(Value::as_str).for_each(|s| {
            text.push_str(s);
            text.push('\n');
        });
    }
    for list in LIST_FIELDS {
        for item in contact
            .get(list)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(s) = item.get("value").and_then(Value::as_str) {
                text.push_str(s);
                text.push('\n');
            }
        }
    }
    text.to_lowercase()
}

fn sanitize(mut value: Value) -> Value {
    strip_sources(&mut value);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("digest");
    }
    value
}

fn valid_patch(patch: &Value) -> bool {
    let Some(obj) = patch.as_object() else {
        return false;
    };
    if !obj
        .keys()
        .all(|k| matches!(k.as_str(), "op" | "path" | "value"))
    {
        return false;
    }
    let (Some(op), Some(path)) = (
        obj.get("op").and_then(Value::as_str),
        obj.get("path").and_then(Value::as_str),
    ) else {
        return false;
    };
    let value = obj.get("value");
    let text_max = if path == "notes" { MAX_NOTES } else { MAX_TEXT };
    let scalar = SCALAR_FIELDS.contains(&path)
        || path
            .strip_prefix("name.")
            .is_some_and(|part| NAME_PARTS.contains(&part));
    if scalar {
        return match op {
            "replace" => value
                .and_then(Value::as_str)
                .is_some_and(|s| s.chars().count() <= text_max),
            "remove" => value.is_none() && path != "display_name",
            _ => false,
        };
    }
    if path == "birthday" {
        return match op {
            "add" | "replace" => value.is_some_and(valid_birthday),
            "remove" => value.is_none(),
            _ => false,
        };
    }
    if LIST_FIELDS.contains(&path) {
        return op == "add" && value.is_some_and(|v| valid_list_item(path, v));
    }
    // `phones[<item id>]` addresses an existing item by its stable id.
    let item = LIST_FIELDS.iter().find_map(|list| {
        path.strip_prefix(list)?
            .strip_prefix('[')?
            .strip_suffix(']')
            .filter(|id| !id.is_empty() && !id.contains(['[', ']']))
            .map(|id| (*list, id))
    });
    match (item, op) {
        (Some((list, id)), "replace") => value.is_some_and(|v| {
            valid_list_item(list, v) && v.get("id").and_then(Value::as_str) == Some(id)
        }),
        (Some(_), "remove") => value.is_none(),
        _ => false,
    }
}

fn patch_root(path: &str) -> &str {
    path.split(['.', '[']).next().unwrap_or(path)
}

/// Shape validation shared by the requester and every receiver. Unknown keys, kinds,
/// patch operations and paths fail closed.
fn validate_request(request: &Value) -> Result<(), Error> {
    schema_v1(request)?;
    let obj = request.as_object().ok_or_else(invalid)?;
    let kind = req(request, "kind")?;
    req(request, "request_id")?;
    req(request, "target_owner")?;
    req(request, "book_id")?;
    let allowed =
        |k: &str| REQUEST_KEYS.contains(&k) || (kind == "create" && CREATE_FIELDS.contains(&k));
    if !obj.keys().all(|k| allowed(k)) {
        return Err(invalid());
    }
    let patches = match obj.get("patches") {
        None => &[][..],
        Some(Value::Array(p)) if p.len() <= MAX_PATCHES && p.iter().all(valid_patch) => {
            p.as_slice()
        }
        Some(_) => return Err(invalid()),
    };
    if let Some(photo) = obj.get("photo_op") {
        let ok = match photo.get("op").and_then(Value::as_str) {
            Some("set") => photo
                .get("attachment_id")
                .is_some_and(|a| a.is_string() || a.is_object()),
            Some("remove") => true,
            _ => false,
        };
        if !ok || kind == "delete" {
            return Err(invalid());
        }
    }
    if obj.get("expires_at").is_some_and(|e| e.as_i64().is_none()) {
        return Err(invalid());
    }
    if obj.get("provenance").is_some_and(|p| !p.is_object()) {
        return Err(invalid());
    }
    match kind {
        "create" => {
            if obj.contains_key("contact_id") {
                req(request, "contact_id")?;
            }
            if !patches.is_empty() || obj.contains_key("expected_old") {
                return Err(invalid());
            }
            validate_fields(request)?;
        }
        "update" => {
            req(request, "contact_id")?;
            revision(request, "base_revision")?;
            if patches.is_empty() && !obj.contains_key("photo_op") {
                return Err(invalid());
            }
            if obj.get("expected_old").is_some_and(|e| !e.is_object()) {
                return Err(invalid());
            }
        }
        "delete" => {
            req(request, "contact_id")?;
            revision(request, "base_revision")?;
            if !patches.is_empty() || obj.contains_key("expected_old") {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    Ok(())
}

pub(crate) fn request_edit(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &str,
) -> Result<String, Error> {
    submit_request(conn, ctx, parse(input)?, false)
}

/// Validates, records and publishes an edit request for the book owner. Only core-built
/// owner-local requests (restore on the owner device) may target this device itself.
pub(crate) fn submit_request(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    mut request: Value,
    allow_self: bool,
) -> Result<String, Error> {
    require_unlocked(ctx)?;
    validate_request(&request)?;
    let id = req(&request, "request_id")?.to_owned();
    let target = req(&request, "target_owner")?.to_owned();
    let book = req(&request, "book_id")?.to_owned();
    if target == ctx.device_id.to_string() && !allow_self {
        return Err(invalid());
    }
    let at = now();
    let expires = request
        .get("expires_at")
        .and_then(Value::as_i64)
        .unwrap_or(at + TTL_SECONDS);
    if expires <= at || expires > at + TTL_SECONDS {
        return Err(invalid());
    }
    request["expires_at"] = json!(expires);
    let tx = conn.transaction()?;
    let owner: Option<String> = tx
        .query_row(
            "SELECT owner_device_id FROM contact_books WHERE id=?",
            [&book],
            |r| r.get(0),
        )
        .optional()?;
    if owner.is_some_and(|owner| owner != target) {
        return Err(invalid());
    }
    let existing: Option<String> = tx
        .query_row(
            "SELECT body FROM contact_edit_ledger WHERE request_id=?",
            [&id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        let existing: Value = serde_json::from_str(&existing).map_err(|_| Error::Database)?;
        let mut same = request.clone();
        same["expires_at"] = existing["expires_at"].clone();
        if existing != same {
            return Err(invalid());
        }
        return Ok(json!({"status":"requested","request_id":id,"duplicate":true}).to_string());
    }
    tx.execute(
        "INSERT INTO contact_edit_ledger(request_id,book_id,requester,body,status,expires_at,key_epoch) VALUES(?,?,?,?,'requested',?,?)",
        params![id, book, ctx.device_id.to_string(), encoded(&request)?, expires, ctx.active_epoch],
    )?;
    enqueue(
        &tx,
        ctx,
        EnvelopePurpose::Event,
        None,
        None,
        &PrivatePayload::ContactEditRequest { request },
    )?;
    tx.commit()?;
    Ok(json!({"status":"requested","request_id":id}).to_string())
}

/// Records a durable owner decision and publishes it to the requester.
fn finalize(
    conn: &Connection,
    ctx: &Ctx<'_>,
    id: &str,
    book_id: &str,
    status: &str,
    reason: Option<&str>,
    observed: Option<Value>,
) -> Result<Value, Error> {
    let mut result = json!({"request_id":id,"book_id":book_id,"status":status});
    if let Some(reason) = reason {
        result["reason"] = json!(reason);
    }
    if let Some(observed) = observed {
        result["observed"] = sanitize(observed);
    }
    conn.execute(
        "UPDATE contact_edit_ledger SET status=?,observed=? WHERE request_id=?",
        params![status, encoded(&result)?, id],
    )?;
    enqueue(
        conn,
        ctx,
        EnvelopePurpose::Event,
        None,
        None,
        &PrivatePayload::ContactEditResult {
            result: result.clone(),
        },
    )?;
    Ok(result)
}

/// `expected` is a subset of `current` (objects compared key-wise, leaves by equality).
fn matches_expected(expected: &Value, current: &Value) -> bool {
    match expected {
        Value::Object(map) => map
            .iter()
            .all(|(k, v)| matches_expected(v, current.get(k).unwrap_or(&Value::Null))),
        other => other == current,
    }
}

fn delete_limit_exceeded(conn: &Connection, book_id: &str, extra: i64) -> Result<bool, Error> {
    let since = now() - ROLLING_WINDOW_SECONDS;
    let recent: i64 = conn.query_row(
        "SELECT COUNT(*) FROM contacts WHERE book_id=? AND deleted_at IS NOT NULL AND deleted_at>=?",
        params![book_id, since],
        |r| r.get(0),
    )?;
    let in_flight = conn
        .prepare("SELECT body FROM contact_edit_ledger WHERE book_id=? AND status IN ('applying','outcome_unknown')")?
        .query_map([book_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .filter(|body| {
            serde_json::from_str::<Value>(body)
                .ok()
                .is_some_and(|b| b.get("kind").and_then(Value::as_str) == Some("delete"))
        })
        .count() as i64;
    let alive: i64 = conn.query_row(
        "SELECT COUNT(*) FROM contacts WHERE book_id=? AND deleted_at IS NULL",
        [book_id],
        |r| r.get(0),
    )?;
    let count = recent + in_flight + extra;
    // Rolling policy: more than 10, or more than 5% of the book, within 24h needs approval.
    Ok(count > DELETE_LIMIT_ABS || count * 100 > (alive + recent) * DELETE_LIMIT_PCT)
}

/// The sole owner-side authorization before a platform write. A permit is issued at most
/// once; asking again after issue reopens the request as `outcome_unknown`.
pub(crate) fn next_permit(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &str,
) -> Result<String, Error> {
    require_unlocked(ctx)?;
    let v = parse(input)?;
    let id = req(&v, "request_id")?;
    let tx = conn.transaction()?;
    let (body, status, expires, issued, book_id, key_epoch): (
        String,
        String,
        i64,
        bool,
        String,
        Option<u32>,
    ) = tx
        .query_row(
            "SELECT body,status,expires_at,permit_issued,book_id,key_epoch FROM contact_edit_ledger WHERE request_id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    let request: Value = serde_json::from_str(&body).map_err(|_| Error::Database)?;
    let (book, _) = owned_book(&tx, ctx, &book_id)?;
    if req(&request, "target_owner")? != ctx.device_id.to_string() {
        return Err(Error::InvalidRequest("not contact edit target"));
    }
    let contact_id = request.get("contact_id").and_then(Value::as_str);
    // Native-only provider mapping (source key, field sources, account attributes).
    let source = |tx: &Connection| crate::contact_source::source_for(tx, &book_id, contact_id);
    if issued {
        if status == "applying" {
            // Published once so the requester also sees the unknown outcome.
            finalize(&tx, ctx, id, &book_id, "outcome_unknown", None, None)?;
        }
        let reopened = matches!(status.as_str(), "applying" | "outcome_unknown");
        let out = if reopened {
            json!({"status":"outcome_unknown","request_id":id,"request":request,"source":source(&tx)?,"evidence":crate::contact_state::evidence(&tx, id)?,"restore":restore_claim(&tx, &book_id, id)?})
        } else {
            json!({"status":status,"request_id":id})
        };
        tx.commit()?;
        return Ok(out.to_string());
    }
    if TERMINAL.contains(&status.as_str()) {
        return Ok(json!({"status":status,"request_id":id}).to_string());
    }
    let decide =
        |status: &str, reason: Option<&str>, observed: Option<Value>| -> Result<String, Error> {
            finalize(&tx, ctx, id, &book_id, status, reason, observed)?;
            Ok(json!({"status":status,"request_id":id,"reason":reason}).to_string())
        };
    if key_epoch != ctx.active_epoch {
        let out = decide("rejected", Some("retired_epoch"), None)?;
        tx.commit()?;
        return Ok(out);
    }
    if expires <= now() {
        let out = decide("expired", None, None)?;
        tx.commit()?;
        return Ok(out);
    }
    let mode = book
        .pointer("/policy/remote_edits")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_REMOTE_EDITS);
    let kind = req(&request, "kind")?;
    let current: Option<(i64, Value)> = match contact_id {
        Some(contact_id) => tx
            .query_row(
                "SELECT revision,body FROM contacts WHERE book_id=? AND id=? AND deleted_at IS NULL",
                params![book_id, contact_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(rev, body)| serde_json::from_str(&body).map(|b| (rev, b)))
            .transpose()
            .map_err(|_| Error::Database)?,
        None => None,
    };
    let source_value = source(&tx)?;
    let rejection = if mode == "off" {
        Some(("rejected", "remote_edits_off"))
    } else if book.pointer("/capabilities/write").and_then(Value::as_bool) == Some(false)
        || source_value
            .pointer("/provenance/read_only")
            .and_then(Value::as_bool)
            == Some(true)
    {
        Some(("rejected", "read_only_account"))
    } else if kind != "create" && current.is_none() {
        Some(("rejected", "not_found"))
    } else if kind == "create" && current.is_some() {
        Some(("conflict", "already_exists"))
    } else if let Some(old) = restored_from(&request) {
        restore_blocker(&tx, &book_id, old, id)?.map(|reason| ("rejected", reason))
    } else {
        None
    };
    if let Some((status, reason)) = rejection {
        let out = decide(status, Some(reason), None)?;
        tx.commit()?;
        return Ok(out);
    }
    if let Some((current_rev, current_body)) = &current
        && revision(&request, "base_revision")? != *current_rev
    {
        // Three-way: a stale base is acceptable only when explicit expected-old values
        // cover every patched field and still match the owner's current content.
        // A photo operation is covered only by an explicit `expected_old.photo`.
        let merged = request.get("expected_old").is_some_and(|expected| {
            request
                .get("patches")
                .and_then(Value::as_array)
                .is_some_and(|patches| {
                    patches
                        .iter()
                        .all(|p| covers(expected, p["path"].as_str().unwrap_or("")))
                })
                && (request.get("photo_op").is_none() || expected.get("photo").is_some())
                && matches_expected(expected, current_body)
        });
        if !merged || kind == "delete" {
            let out = decide("conflict", Some("stale_base"), Some(current_body.clone()))?;
            tx.commit()?;
            return Ok(out);
        }
    }
    let approved = status == "approved";
    let needs_approval = !approved
        && (mode == "confirm" || (kind == "delete" && delete_limit_exceeded(&tx, &book_id, 1)?));
    if needs_approval {
        // Published once; the ledger stays non-terminal until an owner decision.
        if status != "awaiting_approval" {
            finalize(&tx, ctx, id, &book_id, "awaiting_approval", None, None)?;
            tx.commit()?;
        }
        return Ok(json!({"status":"awaiting_approval","request_id":id}).to_string());
    }
    // Media readiness: never authorize an OS write before a set-photo is locally verified.
    // Non-terminal and unrecorded; retry after `install_downloaded_attachment`.
    if let Some(attachment_id) = crate::contact_media::photo_waiting(&tx, &request)? {
        return Ok(
            json!({"status":"waiting_media","request_id":id,"attachment_id":attachment_id})
                .to_string(),
        );
    }
    let issued = tx.execute(
        "UPDATE contact_edit_ledger SET permit_issued=1,status='applying' WHERE request_id=? AND permit_issued=0",
        [id],
    )?;
    if issued != 1 {
        return Err(Error::Database);
    }
    if let Some(old) = restored_from(&request) {
        // Durable claim, atomic with the permit: one OS create per tombstone. A definitive
        // failure releases it; an unknown outcome keeps it.
        let claimed = tx.execute(
            "INSERT INTO contact_restore_claims(book_id,tombstone_id,request_id,status,contact_id) VALUES(?1,?2,?3,'claimed',?4) ON CONFLICT(book_id,tombstone_id) DO UPDATE SET request_id=excluded.request_id,status='claimed',contact_id=excluded.contact_id WHERE contact_restore_claims.status='failed'",
            params![book_id, old, id, uuid::Uuid::new_v4().to_string()],
        )?;
        if claimed != 1 {
            return Err(Error::Database);
        }
    }
    let restore = restore_claim(&tx, &book_id, id)?;
    let book_source = book_source(&tx, &book_id)?;
    tx.commit()?;
    Ok(json!({
        "status":"permit",
        "request_id":id,
        "request":request,
        "source":source_value,
        "book_source":book_source,
        "current":current.map(|(_, body)| sanitize(body)),
        "restore":restore,
    })
    .to_string())
}

/// Records post-write OS evidence. Only `outcome:"applied"` with evidence becomes Applied;
/// `unknown` stays reconcilable and `failed` is terminal.
pub(crate) fn reconcile(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &str,
) -> Result<String, Error> {
    require_unlocked(ctx)?;
    let v = parse(input)?;
    let id = req(&v, "request_id")?;
    let outcome = req(&v, "outcome")?;
    let tx = conn.transaction()?;
    let (body, status, book_id): (String, String, String) = tx
        .query_row(
            "SELECT body,status,book_id FROM contact_edit_ledger WHERE request_id=? AND permit_issued=1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    if !matches!(status.as_str(), "applying" | "outcome_unknown") {
        return Err(Error::InvalidRequest("contact edit already reconciled"));
    }
    let (book, _) = owned_book(&tx, ctx, &book_id)?;
    let request: Value = serde_json::from_str(&body).map_err(|_| Error::Database)?;
    if req(&request, "target_owner")? != ctx.device_id.to_string() {
        return Err(Error::InvalidRequest("not contact edit target"));
    }
    let kind = req(&request, "kind")?;
    let result = match outcome {
        "unknown" => {
            if status == "applying" {
                finalize(&tx, ctx, id, &book_id, "outcome_unknown", None, None)?
            } else {
                json!({"request_id":id,"status":"outcome_unknown"})
            }
        }
        "failed" => {
            tx.execute(
                "UPDATE contact_restore_claims SET status='failed' WHERE request_id=? AND status='claimed'",
                [id],
            )?;
            let reason = v
                .get("reason")
                .and_then(Value::as_str)
                .filter(|r| {
                    !r.is_empty()
                        && r.len() <= 64
                        && r.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                })
                .unwrap_or("platform_failed");
            finalize(&tx, ctx, id, &book_id, "failed", Some(reason), None)?
        }
        "applied" if kind == "delete" => {
            let contact_id = req(&request, "contact_id")?;
            let tombstone = tombstone_owned(&tx, ctx, &book_id, contact_id, now())?;
            finalize(&tx, ctx, id, &book_id, "applied", None, tombstone)?
        }
        "applied" => {
            let mut observed = observed_contact(&tx, &v, "observed", "observed_source", &book_id)?;
            let claim = restore_claim(&tx, &book_id, id)?;
            if let Some(new_id) = claim.get("contact_id").and_then(Value::as_str) {
                // A restore never revives the old id: the legacy DTO takes the claimed new
                // identity; a platform mapping must already be a different identity.
                if v.get("observed").is_some() {
                    observed["id"] = json!(new_id);
                }
            }
            let (contact_id, body, provenance) = native_contact(&observed, &book_id)?;
            if kind == "update"
                && request.get("contact_id").and_then(Value::as_str) != Some(&contact_id)
            {
                return Err(invalid());
            }
            let old = restored_from(&request);
            if old == Some(contact_id.as_str()) {
                return Err(invalid());
            }
            let (stored, _) =
                upsert_owned(&tx, ctx, &book, &book_id, &contact_id, body, provenance)?;
            if let Some(old) = old {
                tx.execute(
                    "UPDATE contact_restore_claims SET status='applied',contact_id=? WHERE request_id=?",
                    params![contact_id, id],
                )?;
                mark_restored(&tx, ctx, &book_id, old, &contact_id)?;
            }
            finalize(&tx, ctx, id, &book_id, "applied", None, Some(stored))?
        }
        _ => return Err(invalid()),
    };
    if outcome == "applied" {
        publish_book_state(&tx, ctx, &book_id)?;
    }
    tx.commit()?;
    Ok(json!({"status":result["status"],"request_id":id}).to_string())
}

/// Every patched path must appear in `expected_old` exactly (`name.given` needs
/// `expected_old.name.given`; list items need the whole list).
fn covers(expected: &Value, path: &str) -> bool {
    match path.split_once('.') {
        Some((root, part)) => expected.get(root).and_then(|r| r.get(part)).is_some(),
        None => expected.get(patch_root(path)).is_some(),
    }
}

fn restored_from(request: &Value) -> Option<&str> {
    (request.get("kind").and_then(Value::as_str) == Some("create"))
        .then(|| {
            request
                .pointer("/provenance/restored_from")
                .and_then(Value::as_str)
        })
        .flatten()
}

/// Reason a restore create cannot proceed: the tombstone is gone/live, already linked to a
/// restored identity, or claimed by another request that has not definitively failed.
fn restore_blocker(
    tx: &Connection,
    book_id: &str,
    old: &str,
    request_id: &str,
) -> Result<Option<&'static str>, Error> {
    let tombstone: Option<String> = tx
        .query_row(
            "SELECT body FROM contacts WHERE book_id=? AND id=? AND deleted_at IS NOT NULL",
            params![book_id, old],
            |r| r.get(0),
        )
        .optional()?;
    let Some(tombstone) = tombstone else {
        return Ok(Some("not_restorable"));
    };
    let tombstone: Value = serde_json::from_str(&tombstone).map_err(|_| Error::Database)?;
    if tombstone.get("restored_to").is_some() {
        return Ok(Some("already_restored"));
    }
    let claim: Option<(String, String)> = tx
        .query_row(
            "SELECT request_id,status FROM contact_restore_claims WHERE book_id=? AND tombstone_id=?",
            params![book_id, old],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match claim {
        Some((claimant, status)) if claimant != request_id && status != "failed" => {
            Some("already_restored")
        }
        _ => None,
    })
}

/// `{restored_from, contact_id}` for this request's restore claim, or `Null`.
fn restore_claim(tx: &Connection, book_id: &str, request_id: &str) -> Result<Value, Error> {
    Ok(tx
        .query_row(
            "SELECT tombstone_id,contact_id FROM contact_restore_claims WHERE book_id=? AND request_id=?",
            params![book_id, request_id],
            |r| Ok(json!({"restored_from": r.get::<_, String>(0)?, "contact_id": r.get::<_, String>(1)?})),
        )
        .optional()?
        .unwrap_or(Value::Null))
}

/// Raw native book (accounts, default account) for owner-local platform writes only.
pub(crate) fn book_source(conn: &Connection, book_id: &str) -> Result<Value, Error> {
    conn.query_row(
        "SELECT body FROM contact_book_sources WHERE book_id=?",
        [book_id],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map_or(Ok(Value::Null), |body| {
        serde_json::from_str(&body).map_err(|_| Error::Database)
    })
}

/// Exactly one of the legacy contact DTO (`legacy` key) or a platform source entry
/// (`platform` key, mapped to core ids in this transaction).
fn observed_contact(
    tx: &Connection,
    v: &Value,
    legacy: &str,
    platform: &str,
    book_id: &str,
) -> Result<Value, Error> {
    match (v.get(legacy), v.get(platform)) {
        (Some(contact), None) => Ok(contact.clone()),
        (None, Some(source)) => Ok(crate::contact_source::platform_contact(tx, book_id, source)?.1),
        _ => Err(invalid()),
    }
}

/// Owner decision for an edit request (`request_id`) or a held scan deletion (`scan_id`).
/// Rejections publish an owner-authored result; decided rows never change again.
pub(crate) fn decide_approval(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &Value,
) -> Result<String, Error> {
    require_unlocked(ctx)?;
    let approve = input
        .get("approve")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let tx = conn.transaction()?;
    let out = match (input.get("request_id"), input.get("scan_id")) {
        (Some(_), None) => decide_request(&tx, ctx, req(input, "request_id")?, approve)?,
        (None, Some(_)) => decide_hold(&tx, ctx, req(input, "scan_id")?, approve)?,
        _ => return Err(invalid()),
    };
    tx.commit()?;
    Ok(out.to_string())
}

fn decide_request(tx: &Connection, ctx: &Ctx<'_>, id: &str, approve: bool) -> Result<Value, Error> {
    let (book_id, status, issued): (String, String, bool) = tx
        .query_row(
            "SELECT book_id,status,permit_issued FROM contact_edit_ledger WHERE request_id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    owned_book(tx, ctx, &book_id)?;
    if issued || !matches!(status.as_str(), "requested" | "awaiting_approval") {
        return Ok(json!({"schema_version":1,"status":"unchanged","request_id":id,"state":status}));
    }
    if approve {
        tx.execute(
            "UPDATE contact_edit_ledger SET status='approved' WHERE request_id=?",
            [id],
        )?;
    } else {
        finalize(
            tx,
            ctx,
            id,
            &book_id,
            "rejected",
            Some("owner_rejected"),
            None,
        )?;
    }
    Ok(json!({"schema_version":1,"request_id":id,"status":"decided","approved":approve}))
}

fn decide_hold(tx: &Connection, ctx: &Ctx<'_>, scan: &str, approve: bool) -> Result<Value, Error> {
    let (book_id, generation, members, status): (String, i64, String, String) = tx
        .query_row(
            "SELECT book_id,generation,members,status FROM contact_scan_holds WHERE scan_id=?",
            [scan],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    let (_, current_generation) = owned_book(tx, ctx, &book_id)?;
    if status != "awaiting_approval" {
        return Ok(json!({"schema_version":1,"status":"unchanged","scan_id":scan,"state":status}));
    }
    let mut deleted = 0;
    let state = if !approve {
        "rejected"
    } else if current_generation != generation {
        "stale"
    } else {
        // Projection-only removal: the OS already lacks these contacts. A contact
        // re-observed or edited since the scan has a newer revision and is kept.
        let members: Vec<Value> = serde_json::from_str(&members).map_err(|_| Error::Database)?;
        let at = now();
        for member in &members {
            let id = req(member, "id")?;
            let current: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM contacts WHERE book_id=? AND id=? AND deleted_at IS NULL",
                    params![book_id, id],
                    |r| r.get(0),
                )
                .optional()?;
            if current == Some(revision(member, "revision")?)
                && tombstone_owned(tx, ctx, &book_id, id, at)?.is_some()
            {
                deleted += 1;
            }
        }
        "applied"
    };
    tx.execute(
        "UPDATE contact_scan_holds SET status=? WHERE scan_id=? AND status='awaiting_approval'",
        params![state, scan],
    )?;
    if deleted > 0 {
        publish_book_state(tx, ctx, &book_id)?;
    }
    Ok(
        json!({"schema_version":1,"scan_id":scan,"status":"decided","approved":approve,"state":state,"deleted":deleted}),
    )
}

fn contact_name(conn: &Connection, book_id: &str, contact_id: &str) -> Result<Value, Error> {
    let body: Option<String> = conn
        .query_row(
            "SELECT body FROM contacts WHERE book_id=? AND id=?",
            params![book_id, contact_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(body
        .and_then(|b| serde_json::from_str::<Value>(&b).ok())
        .and_then(|b| b.get("display_name").cloned())
        .unwrap_or(Value::Null))
}

/// Sanitized approval summary of a ledger request: kind, target, display name and the
/// field paths it touches (never patch values or provider identity).
pub(crate) fn request_summary(
    conn: &Connection,
    book_id: &str,
    request: &Value,
) -> Result<Value, Error> {
    let kind = request.get("kind").cloned().unwrap_or(Value::Null);
    let contact_id = request.get("contact_id").and_then(Value::as_str);
    let mut paths = request
        .get("patches")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("path").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if kind == "create" {
        paths.extend(
            CREATE_FIELDS
                .iter()
                .filter(|f| request.get(**f).is_some())
                .map(|f| (*f).to_owned()),
        );
    }
    if request.get("photo_op").is_some() {
        paths.push("photo".to_owned());
    }
    paths.truncate(MAX_PATCHES);
    let display_name = match (kind.as_str(), contact_id) {
        (Some("create"), _) => request.get("display_name").cloned().unwrap_or(Value::Null),
        (_, Some(id)) => contact_name(conn, book_id, id)?,
        _ => Value::Null,
    };
    Ok(
        json!({"kind": kind, "contact_id": contact_id, "display_name": display_name, "field_paths": paths}),
    )
}

/// Durable scan-deletion holds awaiting this owner's decision (survive restart).
pub(crate) fn pending_holds(
    conn: &Connection,
    owner: &str,
    book: Option<&str>,
    limit: i64,
) -> Result<Vec<Value>, Error> {
    let rows = conn
        .prepare("SELECT h.scan_id,h.book_id,h.members,h.created_at FROM contact_scan_holds h JOIN contact_books b ON b.id=h.book_id WHERE h.status='awaiting_approval' AND b.owner_device_id=?1 AND b.forgotten=0 AND (?2 IS NULL OR h.book_id=?2) ORDER BY h.created_at DESC, h.scan_id LIMIT ?3")?
        .query_map(params![owner, book, limit], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(scan_id, book_id, members, created_at)| {
            let members: Vec<Value> =
                serde_json::from_str(&members).map_err(|_| Error::Database)?;
            let mut sample = Vec::new();
            for member in members.iter().take(5) {
                if let Some(id) = member.get("id").and_then(Value::as_str) {
                    sample.push(contact_name(conn, &book_id, id)?);
                }
            }
            Ok(json!({
                "kind": "scan_deletions",
                "scan_id": scan_id,
                "book_id": book_id,
                "state": "awaiting_approval",
                "count": members.len(),
                "sample_names": sample,
                "created_at": created_at,
            }))
        })
        .collect()
}

pub(crate) fn forget(conn: &Connection, input: &str) -> Result<String, Error> {
    let v = parse(input)?;
    let id = req(&v, "book_id")?;
    conn.execute("UPDATE contact_books SET forgotten=1 WHERE id=?", [id])?;
    Ok(json!({"status":"forgotten","book_id":id}).to_string())
}

/// Applies a peer contact payload. `Ok(false)` quarantines the record; malformed or
/// unauthorized payloads never surface as `Err`, which would wedge the apply batch.
pub(crate) fn apply_event(
    conn: &Connection,
    producer: &str,
    historical: bool,
    key_epoch: u32,
    payload: &PrivatePayload,
) -> Result<bool, Error> {
    match apply_peer(conn, producer, historical, key_epoch, payload) {
        Err(Error::InvalidRequest(_)) => Ok(false),
        other => other,
    }
}

fn stored_owner(conn: &Connection, book_id: &str) -> Result<Option<String>, Error> {
    Ok(conn
        .query_row(
            "SELECT owner_device_id FROM contact_books WHERE id=?",
            [book_id],
            |r| r.get(0),
        )
        .optional()?)
}

fn apply_peer(
    conn: &Connection,
    producer: &str,
    historical: bool,
    key_epoch: u32,
    payload: &PrivatePayload,
) -> Result<bool, Error> {
    match payload {
        PrivatePayload::ContactBookState { book } => apply_book(conn, producer, book),
        PrivatePayload::ContactUpserted { book, contact } => {
            if !apply_book(conn, producer, book)? {
                return Ok(false);
            }
            let (id, rev, body) = peer_contact(contact, req(book, "id")?)?;
            conn.execute(
                "INSERT INTO contacts(id,book_id,revision,body,deleted_at) VALUES(?,?,?,?,NULL) ON CONFLICT(book_id,id) DO UPDATE SET revision=excluded.revision,body=excluded.body,deleted_at=NULL WHERE excluded.revision > contacts.revision",
                params![id, req(book, "id")?, rev, encoded(&body)?],
            )?;
            Ok(true)
        }
        PrivatePayload::ContactRemoved {
            book_id,
            contact_id,
            tombstone,
        } => {
            if req(tombstone, "contact_id")? != contact_id {
                return Ok(false);
            }
            if let Some(book) = tombstone.get("book")
                && (req(book, "id")? != book_id || !apply_book(conn, producer, book)?)
            {
                return Ok(false);
            }
            if stored_owner(conn, book_id)?.as_deref() != Some(producer) {
                return Ok(false);
            }
            let rev = revision(tombstone, "revision")?;
            let deleted_at = tombstone
                .get("deleted_at")
                .and_then(Value::as_i64)
                .ok_or_else(invalid)?;
            let restored = tombstone.get("restored_contact").ok_or_else(invalid)?;
            let (id, body_rev, body) = peer_contact(restored, book_id)?;
            if id != *contact_id || body_rev != rev {
                return Ok(false);
            }
            conn.execute(
                "INSERT INTO contacts(id,book_id,revision,body,deleted_at) VALUES(?,?,?,?,?) ON CONFLICT(book_id,id) DO UPDATE SET revision=excluded.revision,body=excluded.body,deleted_at=excluded.deleted_at WHERE excluded.revision > contacts.revision",
                params![id, book_id, rev, encoded(&body)?, deleted_at],
            )?;
            Ok(true)
        }
        PrivatePayload::ContactEditRequest { request } => {
            // Historical snapshot requests never authorize OS writes: no ledger row, no permit.
            if historical {
                return Ok(true);
            }
            validate_request(request)?;
            let target = req(request, "target_owner")?;
            let book_id = req(request, "book_id")?;
            let expires = request
                .get("expires_at")
                .and_then(Value::as_i64)
                .ok_or_else(invalid)?;
            // Owner-local requests (e.g. restore) are mirrored but never actionable here.
            if target == producer {
                return Ok(true);
            }
            if expires > now() + TTL_SECONDS + EXPIRY_SKEW_SECONDS
                || stored_owner(conn, book_id)?.is_some_and(|owner| owner != target)
            {
                return Ok(false);
            }
            conn.execute(
                "INSERT INTO contact_edit_ledger(request_id,book_id,requester,body,status,expires_at,key_epoch) VALUES(?,?,?,?,'requested',?,?) ON CONFLICT(request_id) DO NOTHING",
                params![req(request, "request_id")?, book_id, producer, encoded(request)?, expires, key_epoch],
            )?;
            Ok(true)
        }
        PrivatePayload::ContactEditResult { result } => {
            let id = req(result, "request_id")?;
            let status = req(result, "status")?;
            if !RESULT_STATUSES.contains(&status) {
                return Ok(false);
            }
            let row: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT body,status,book_id FROM contact_edit_ledger WHERE request_id=?",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            // A result for a request this device never saw has nothing to bind to.
            let Some((body, current, book_id)) = row else {
                return Ok(true);
            };
            let request: Value = serde_json::from_str(&body).map_err(|_| Error::Database)?;
            if req(&request, "target_owner")? != producer
                || stored_owner(conn, &book_id)?.is_some_and(|owner| owner != producer)
                || result
                    .get("book_id")
                    .and_then(Value::as_str)
                    .is_some_and(|b| b != book_id)
            {
                return Ok(false);
            }
            if TERMINAL.contains(&current.as_str()) {
                return Ok(true);
            }
            let mut result = result.clone();
            strip_sources(&mut result);
            conn.execute(
                "UPDATE contact_edit_ledger SET status=?,observed=? WHERE request_id=?",
                params![status, encoded(&result)?, id],
            )?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Book state from its owner. A foreign owner quarantines; an older generation is
/// accepted as a no-op (rev-gated contacts may still apply) but never rolls state back.
fn apply_book(conn: &Connection, producer: &str, book: &Value) -> Result<bool, Error> {
    let id = req(book, "id")?;
    if req(book, "owner_device_id")? != producer {
        return Ok(false);
    }
    let generation = revision(book, "generation")?;
    book_state_revision(book)?;
    let stored: Option<(String, i64, String)> = conn
        .query_row(
            "SELECT owner_device_id,generation,body FROM contact_books WHERE id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((owner, stored_generation, stored_body)) = stored {
        if owner != producer {
            return Ok(false);
        }
        let stored_body: Value = serde_json::from_str(&stored_body).map_err(|_| Error::Database)?;
        // Ownership is valid; an older or equal state is accepted as a no-op.
        if !book_supersedes(stored_generation, &stored_body, generation, book)? {
            return Ok(true);
        }
    }
    let mut body = book.clone();
    strip_sources(&mut body);
    conn.execute(
        "INSERT INTO contact_books(id,owner_device_id,generation,state,body,forgotten) VALUES(?,?,?,?,?,0) ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,state=excluded.state,body=excluded.body",
        params![
            id,
            producer,
            generation,
            book.get("state").and_then(Value::as_str).unwrap_or("active"),
            encoded(&body)?
        ],
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn
    }
    fn book(owner: &str, generation: &str) -> Value {
        json!({"id":"b1","owner_device_id":owner,"generation":generation,"state":"active"})
    }
    fn contact(rev: &str, name: &str) -> Value {
        json!({"id":"c1","book_id":"b1","revision":rev,"display_name":name})
    }
    fn upsert(rev: &str, name: &str) -> PrivatePayload {
        PrivatePayload::ContactUpserted {
            book: book("owner", "1"),
            contact: contact(rev, name),
        }
    }
    fn name(conn: &Connection) -> (String, Option<i64>) {
        conn.query_row(
            "SELECT body,deleted_at FROM contacts WHERE id='c1'",
            [],
            |r| {
                Ok((
                    serde_json::from_str::<Value>(&r.get::<_, String>(0)?).unwrap()["display_name"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    r.get(1)?,
                ))
            },
        )
        .unwrap()
    }

    #[test]
    fn malformed_peer_payloads_quarantine_instead_of_erroring() {
        let conn = db();
        let bad = PrivatePayload::ContactUpserted {
            book: json!({"id":"b1"}),
            contact: json!({}),
        };
        assert!(!apply_event(&conn, "owner", false, 1, &bad).unwrap());
        let bad_rev = PrivatePayload::ContactUpserted {
            book: book("owner", "1"),
            contact: contact("01", "A"),
        };
        assert!(!apply_event(&conn, "owner", false, 1, &bad_rev).unwrap());
        let bad_request = PrivatePayload::ContactEditRequest {
            request: json!({"schema_version":1}),
        };
        assert!(!apply_event(&conn, "peer", false, 1, &bad_request).unwrap());
    }

    #[test]
    fn foreign_owner_and_cross_book_contacts_are_rejected() {
        let conn = db();
        assert!(apply_event(&conn, "owner", false, 1, &upsert("1", "A")).unwrap());
        // Another device claiming the same book id is quarantined and changes nothing.
        let hijack = PrivatePayload::ContactUpserted {
            book: book("mallory", "9"),
            contact: contact("9", "M"),
        };
        assert!(!apply_event(&conn, "mallory", false, 1, &hijack).unwrap());
        // A contact naming another book inside an owned book's event is rejected.
        let cross = PrivatePayload::ContactUpserted {
            book: book("owner", "1"),
            contact: json!({"id":"c1","book_id":"b2","revision":"5","display_name":"X"}),
        };
        assert!(!apply_event(&conn, "owner", false, 1, &cross).unwrap());
        assert_eq!(name(&conn).0, "A");
    }

    #[test]
    fn tombstones_and_upserts_are_revision_gated() {
        let conn = db();
        assert!(apply_event(&conn, "owner", false, 1, &upsert("3", "Three")).unwrap());
        let stale = PrivatePayload::ContactRemoved {
            book_id: "b1".into(),
            contact_id: "c1".into(),
            tombstone: json!({"contact_id":"c1","revision":"2","deleted_at":1,"restored_contact":contact("2","Two")}),
        };
        assert!(apply_event(&conn, "owner", false, 1, &stale).unwrap());
        assert_eq!(name(&conn), ("Three".into(), None));
        let fresh = PrivatePayload::ContactRemoved {
            book_id: "b1".into(),
            contact_id: "c1".into(),
            tombstone: json!({"contact_id":"c1","revision":"4","deleted_at":7,"restored_contact":contact("4","Three")}),
        };
        assert!(apply_event(&conn, "owner", false, 1, &fresh).unwrap());
        assert_eq!(name(&conn), ("Three".into(), Some(7)));
        // A replayed older upsert cannot resurrect the tombstoned contact.
        assert!(apply_event(&conn, "owner", false, 1, &upsert("3", "Three")).unwrap());
        assert_eq!(name(&conn).1, Some(7));
        // A non-owner tombstone is quarantined.
        assert!(!apply_event(&conn, "mallory", false, 1, &fresh).unwrap());
    }

    #[test]
    fn edit_results_bind_to_the_target_owner_and_stay_terminal() {
        let conn = db();
        assert!(
            apply_event(
                &conn,
                "owner",
                false,
                1,
                &PrivatePayload::ContactBookState {
                    book: book("owner", "1")
                }
            )
            .unwrap()
        );
        let request = json!({"schema_version":1,"request_id":"r1","target_owner":"owner","book_id":"b1","kind":"delete","contact_id":"c1","base_revision":"1"});
        conn.execute(
            "INSERT INTO contact_edit_ledger(request_id,book_id,requester,body,status,expires_at) VALUES('r1','b1','me',?,'requested',?)",
            params![request.to_string(), now() + 60],
        )
        .unwrap();
        let status = |conn: &Connection| -> String {
            conn.query_row(
                "SELECT status FROM contact_edit_ledger WHERE request_id='r1'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        let result = |status: &str| PrivatePayload::ContactEditResult {
            result: json!({"request_id":"r1","book_id":"b1","status":status}),
        };
        assert!(!apply_event(&conn, "mallory", false, 1, &result("applied")).unwrap());
        assert_eq!(status(&conn), "requested");
        assert!(!apply_event(&conn, "owner", false, 1, &result("approved_by_magic")).unwrap());
        assert!(apply_event(&conn, "owner", false, 1, &result("rejected")).unwrap());
        assert!(apply_event(&conn, "owner", false, 1, &result("applied")).unwrap());
        assert_eq!(status(&conn), "rejected");
    }

    #[test]
    fn historical_requests_never_create_ledger_rows() {
        let conn = db();
        let request = json!({"schema_version":1,"request_id":"r1","target_owner":"owner","book_id":"b1","kind":"delete","contact_id":"c1","base_revision":"1","expires_at":now()+60});
        assert!(
            apply_event(
                &conn,
                "peer",
                true,
                1,
                &PrivatePayload::ContactEditRequest { request }
            )
            .unwrap()
        );
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM contact_edit_ledger", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn unknown_patch_operations_and_paths_fail_closed() {
        let base = json!({"schema_version":1,"request_id":"r","target_owner":"o","book_id":"b","kind":"update","contact_id":"c","base_revision":"1"});
        let with = |patches: Value| {
            let mut r = base.clone();
            r["patches"] = patches;
            validate_request(&r)
        };
        assert!(with(json!([{"op":"replace","path":"name.given","value":"A"}])).is_ok());
        assert!(
            with(json!([{"op":"replace","path":"phones[p1]","value":{"id":"p1","value":"+1"}}]))
                .is_ok()
        );
        assert!(with(json!([{"op":"move","path":"name.given","value":"A"}])).is_err());
        assert!(with(json!([{"op":"replace","path":"provenance.source_id","value":"x"}])).is_err());
        assert!(with(json!([{"op":"replace","path":"name.given","value":"A","extra":1}])).is_err());
        assert!(with(json!([])).is_err());
        let mut unknown_key = base.clone();
        unknown_key["patches"] = json!([{"op":"remove","path":"nickname"}]);
        unknown_key["surprise"] = json!(true);
        assert!(validate_request(&unknown_key).is_err());
    }
}
