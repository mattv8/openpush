//! Owner-local identity mappings for native contact providers.
use crate::{Ctx, Error, contacts};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

const MAX_BATCH: usize = 200;
const LISTS: [&str; 3] = ["phones", "emails", "addresses"];

fn invalid() -> Error {
    Error::InvalidRequest("invalid contact source request")
}
fn parse(input: &str) -> Result<Value, Error> {
    serde_json::from_str(input).map_err(|_| invalid())
}
fn req<'a>(v: &'a Value, key: &str) -> Result<&'a str, Error> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 1024)
        .ok_or_else(invalid)
}
fn schema(v: &Value) -> Result<(), Error> {
    if v.get("schema_version").and_then(Value::as_u64) == Some(1) {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn object_keys<'a>(v: &'a Value, allowed: &[&str]) -> Result<&'a Map<String, Value>, Error> {
    let map = v.as_object().ok_or_else(invalid)?;
    if map.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(map)
    } else {
        Err(invalid())
    }
}

pub(crate) fn initialize(conn: &Connection) -> Result<(), Error> {
    // The single owner-local source mapping: provider contact key -> core contact id,
    // provider item id -> core field id, and provider account attributes. None of these
    // tables is ever projected, enqueued or shown to UI.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS contact_source_map(book_id TEXT NOT NULL, source_key TEXT NOT NULL, contact_id TEXT NOT NULL, PRIMARY KEY(book_id,source_key), UNIQUE(book_id,contact_id));
         CREATE TABLE IF NOT EXISTS contact_source_field_map(book_id TEXT NOT NULL, contact_id TEXT NOT NULL, field TEXT NOT NULL, provider_item_id TEXT NOT NULL, field_id TEXT NOT NULL, PRIMARY KEY(book_id,contact_id,field,provider_item_id), UNIQUE(book_id,contact_id,field,field_id));
         CREATE TABLE IF NOT EXISTS contact_sources(book_id TEXT NOT NULL, contact_id TEXT NOT NULL, provenance TEXT NOT NULL, PRIMARY KEY(book_id,contact_id));"
    )?;
    Ok(())
}

/// Records provider attributes for an owner contact. A legacy `provenance.source_id`
/// becomes the contact's source key, so both capture APIs share one mapping.
pub(crate) fn record_source(
    conn: &Connection,
    book_id: &str,
    contact_id: &str,
    provenance: Option<Value>,
) -> Result<(), Error> {
    let Some(mut provenance) = provenance else {
        return Ok(());
    };
    let attrs = provenance.as_object_mut().ok_or_else(invalid)?;
    if let Some(source_key) = attrs.remove("source_id") {
        let source_key = source_key
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 1024)
            .ok_or_else(invalid)?;
        conn.execute(
            "INSERT INTO contact_source_map(book_id,source_key,contact_id) VALUES(?,?,?) ON CONFLICT DO NOTHING",
            params![book_id, source_key, contact_id],
        )?;
        let mapped: Option<String> = conn
            .query_row(
                "SELECT contact_id FROM contact_source_map WHERE book_id=? AND source_key=?",
                params![book_id, source_key],
                |r| r.get(0),
            )
            .optional()?;
        if mapped.as_deref() != Some(contact_id) {
            return Err(invalid());
        }
    }
    conn.execute(
        "INSERT INTO contact_sources(book_id,contact_id,provenance) VALUES(?,?,?) ON CONFLICT(book_id,contact_id) DO UPDATE SET provenance=excluded.provenance",
        params![book_id, contact_id, serde_json::to_string(&provenance).map_err(|_| Error::Database)?],
    )?;
    Ok(())
}

/// Native-only source context for an owner contact: `{source_key, field_sources,
/// provenance}`; `Null` when the contact has no mapping (or no contact id is given).
pub(crate) fn source_for(
    conn: &Connection,
    book_id: &str,
    contact_id: Option<&str>,
) -> Result<Value, Error> {
    let Some(contact_id) = contact_id else {
        return Ok(Value::Null);
    };
    let source_key: Option<String> = conn
        .query_row(
            "SELECT source_key FROM contact_source_map WHERE book_id=? AND contact_id=?",
            params![book_id, contact_id],
            |r| r.get(0),
        )
        .optional()?;
    let provenance: Option<String> = conn
        .query_row(
            "SELECT provenance FROM contact_sources WHERE book_id=? AND contact_id=?",
            params![book_id, contact_id],
            |r| r.get(0),
        )
        .optional()?;
    let field_sources = conn
        .prepare("SELECT field,provider_item_id,field_id FROM contact_source_field_map WHERE book_id=? AND contact_id=? ORDER BY field,field_id")?
        .query_map(params![book_id, contact_id], |r| {
            Ok(json!({"field": r.get::<_, String>(0)?, "source_id": r.get::<_, String>(1)?, "id": r.get::<_, String>(2)?}))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if source_key.is_none() && provenance.is_none() && field_sources.is_empty() {
        return Ok(Value::Null);
    }
    let provenance = provenance
        .map(|p| serde_json::from_str::<Value>(&p).map_err(|_| Error::Database))
        .transpose()?
        .unwrap_or(Value::Null);
    Ok(json!({"source_key": source_key, "field_sources": field_sources, "provenance": provenance}))
}

/// Maps one platform entry `{source_key, fields, provenance?}` to a core contact DTO in
/// the caller's transaction (minting stable contact and field ids on first sight).
pub(crate) fn platform_contact(
    tx: &Connection,
    book_id: &str,
    source: &Value,
) -> Result<(String, Value), Error> {
    object_keys(source, &["source_key", "fields", "provenance"])?;
    let source_key = req(source, "source_key")?.to_owned();
    migrate_legacy_key(tx, book_id, &source_key, source.get("provenance"))?;
    let id = map_contact(tx, book_id, &source_key)?;
    let fields = clean_fields(tx, book_id, &id, source.get("fields").ok_or_else(invalid)?)?;
    let mut contact = fields.as_object().ok_or_else(invalid)?.clone();
    contact.insert("id".to_owned(), json!(id));
    contact.insert("book_id".to_owned(), json!(book_id));
    // Hosts capture text first and photos lazily: an omitted `photo` keeps the stored one,
    // an explicit `null` removes it, and any other value replaces it.
    let photo_given = contact.contains_key("photo");
    match contact.get("photo") {
        None => {
            if let Some(photo) = stored_photo(tx, book_id, &id)? {
                contact.insert("photo".to_owned(), photo);
            }
        }
        Some(Value::Null) => {
            contact.remove("photo");
        }
        Some(_) => {}
    }
    if let Some(provenance) =
        merged_provenance(tx, book_id, &id, source.get("provenance"), photo_given)?
    {
        contact.insert("provenance".to_owned(), provenance);
    }
    Ok((source_key, Value::Object(contact)))
}

const PROVENANCE_KEYS: [&str; 5] = [
    "account_id",
    "read_only",
    "photo_source_hash",
    "raw_ids",
    "lookup_hint",
];
const MAX_PHOTO_SOURCE_HASH: usize = 128;
/// Owner-local provider raw identities of one captured contact (e.g. Android raw contact ids).
const MAX_RAW_IDS: usize = 64;

/// `raw_ids`: non-empty array of distinct canonical decimal strings; `lookup_hint`: bounded
/// opaque string. Both are owner-local provenance only.
fn valid_raw_identity(given: &Map<String, Value>) -> bool {
    let raw_ids_ok = given.get("raw_ids").is_none_or(|ids| {
        ids.is_null()
            || ids.as_array().is_some_and(|ids| {
                let mut seen = std::collections::HashSet::new();
                !ids.is_empty()
                    && ids.len() <= MAX_RAW_IDS
                    && ids.iter().all(|id| {
                        id.as_str().is_some_and(|id| {
                            !id.is_empty()
                                && id.len() <= 20
                                && id.bytes().all(|b| b.is_ascii_digit())
                                && (id == "0" || !id.starts_with('0'))
                                && seen.insert(id.to_owned())
                        })
                    })
            })
    });
    let hint_ok = given.get("lookup_hint").is_none_or(|hint| {
        hint.is_null()
            || hint
                .as_str()
                .is_some_and(|h| !h.is_empty() && h.len() <= 1024)
    });
    raw_ids_ok && hint_ok
}

/// One-time migration of a mapping created before raw-identity source keys. When a new
/// source key is unmapped and the entry carries raw identity plus a `lookup_hint` that is
/// *exactly* an existing source key whose stored provenance has no `raw_ids` (a legacy,
/// lookup-keyed mapping), that mapping is renamed to the new key so the core contact keeps
/// its id. Nothing is guessed: no fuzzy match, no rename of raw-identity mappings.
fn migrate_legacy_key(
    tx: &Connection,
    book_id: &str,
    source_key: &str,
    provenance: Option<&Value>,
) -> Result<(), Error> {
    let Some(provenance) = provenance else {
        return Ok(());
    };
    let (Some(hint), true) = (
        provenance.get("lookup_hint").and_then(Value::as_str),
        provenance.get("raw_ids").is_some_and(Value::is_array),
    ) else {
        return Ok(());
    };
    if hint == source_key {
        return Ok(());
    }
    let mapped = |key: &str| -> Result<Option<String>, Error> {
        Ok(tx
            .query_row(
                "SELECT contact_id FROM contact_source_map WHERE book_id=? AND source_key=?",
                params![book_id, key],
                |r| r.get(0),
            )
            .optional()?)
    };
    if mapped(source_key)?.is_some() {
        return Ok(());
    }
    let Some(legacy) = mapped(hint)? else {
        return Ok(());
    };
    let stored: Option<String> = tx
        .query_row(
            "SELECT provenance FROM contact_sources WHERE book_id=? AND contact_id=?",
            params![book_id, legacy],
            |r| r.get(0),
        )
        .optional()?;
    let has_raw_identity = stored
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|p| p.get("raw_ids").is_some());
    if has_raw_identity {
        return Ok(());
    }
    tx.execute(
        "UPDATE contact_source_map SET source_key=? WHERE book_id=? AND source_key=?",
        params![source_key, book_id, hint],
    )?;
    Ok(())
}

fn stored_photo(tx: &Connection, book_id: &str, contact_id: &str) -> Result<Option<Value>, Error> {
    let body: Option<String> = tx
        .query_row(
            "SELECT body FROM contacts WHERE book_id=? AND id=?",
            params![book_id, contact_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(body) = body else {
        return Ok(None);
    };
    let body: Value = serde_json::from_str(&body).map_err(|_| Error::Database)?;
    Ok(body.get("photo").filter(|p| !p.is_null()).cloned())
}

/// Owner-local provider attributes merged over the stored ones, so partial captures never
/// erase them. `photo_source_hash` (an opaque native fingerprint of the provider photo bytes)
/// is kept while the photo is inherited, replaced when given, and cleared when the photo
/// changes without a new hash. Provenance is local-only: never projected, synced or shown.
fn merged_provenance(
    tx: &Connection,
    book_id: &str,
    contact_id: &str,
    given: Option<&Value>,
    photo_given: bool,
) -> Result<Option<Value>, Error> {
    let stored: Option<String> = tx
        .query_row(
            "SELECT provenance FROM contact_sources WHERE book_id=? AND contact_id=?",
            params![book_id, contact_id],
            |r| r.get(0),
        )
        .optional()?;
    let mut merged = match &stored {
        Some(text) => match serde_json::from_str::<Value>(text).map_err(|_| Error::Database)? {
            Value::Object(map) => map,
            _ => Map::new(),
        },
        None => Map::new(),
    };
    let original = merged.clone();
    let given = match given {
        Some(given) => {
            // Attributes only; the source key is already mapped by `platform_contact`.
            let given = object_keys(given, &PROVENANCE_KEYS)?;
            if let Some(hash) = given.get("photo_source_hash") {
                let ok = hash.is_null()
                    || hash
                        .as_str()
                        .is_some_and(|h| !h.is_empty() && h.len() <= MAX_PHOTO_SOURCE_HASH);
                if !ok {
                    return Err(invalid());
                }
            }
            if !valid_raw_identity(given) {
                return Err(invalid());
            }
            Some(given)
        }
        None => None,
    };
    for (key, value) in given.into_iter().flatten() {
        if value.is_null() {
            merged.remove(key);
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }
    if photo_given && !given.is_some_and(|g| g.contains_key("photo_source_hash")) {
        merged.remove("photo_source_hash");
    }
    if given.is_none() && merged == original {
        return Ok(None);
    }
    Ok(Some(Value::Object(merged)))
}

fn map_contact(tx: &Connection, book_id: &str, source_key: &str) -> Result<String, Error> {
    if let Some(id) = tx
        .query_row(
            "SELECT contact_id FROM contact_source_map WHERE book_id=? AND source_key=?",
            params![book_id, source_key],
            |r| r.get(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO contact_source_map(book_id,source_key,contact_id) VALUES(?,?,?)",
        params![book_id, source_key, id],
    )?;
    Ok(id)
}
fn map_field(
    tx: &Connection,
    book_id: &str,
    contact_id: &str,
    field: &str,
    source_id: &str,
) -> Result<String, Error> {
    if let Some(id) = tx.query_row("SELECT field_id FROM contact_source_field_map WHERE book_id=? AND contact_id=? AND field=? AND provider_item_id=?", params![book_id, contact_id, field, source_id], |r| r.get(0)).optional()? {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    tx.execute("INSERT INTO contact_source_field_map(book_id,contact_id,field,provider_item_id,field_id) VALUES(?,?,?,?,?)", params![book_id, contact_id, field, source_id, id])?;
    Ok(id)
}
fn clean_fields(
    tx: &Connection,
    book_id: &str,
    contact_id: &str,
    fields: &Value,
) -> Result<Value, Error> {
    object_keys(
        fields,
        &[
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
            "birthday",
        ],
    )?;
    let mut fields = fields.clone();
    let obj = fields.as_object_mut().ok_or_else(invalid)?;
    for field in LISTS {
        let Some(items) = obj.get_mut(field) else {
            continue;
        };
        let items = items.as_array_mut().ok_or_else(invalid)?;
        for item in items {
            let source_id = req(item, "id")?.to_owned();
            let core_id = map_field(tx, book_id, contact_id, field, &source_id)?;
            item.as_object_mut()
                .ok_or_else(invalid)?
                .insert("id".to_owned(), Value::String(core_id));
        }
    }
    Ok(fields)
}

/// Platform capture: maps provider identity to core ids and captures through the same
/// transaction as the encrypted book/contact events.
pub(crate) fn capture(conn: &mut Connection, ctx: &Ctx<'_>, input: &str) -> Result<String, Error> {
    let input = parse(input)?;
    schema(&input)?;
    object_keys(&input, &["schema_version", "book", "contacts"])?;
    let book = input.get("book").ok_or_else(invalid)?;
    let book_id = req(book, "id")?.to_owned();
    if req(book, "owner_device_id")? != ctx.device_id.to_string() {
        return Err(invalid());
    }
    let incoming = input
        .get("contacts")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if incoming.len() > MAX_BATCH {
        return Err(invalid());
    }
    let tx = conn.transaction()?;
    // A remote (non-owned) book id must fail before any mapping row is written.
    let owner: Option<String> = tx
        .query_row(
            "SELECT owner_device_id FROM contact_books WHERE id=?",
            [&book_id],
            |r| r.get(0),
        )
        .optional()?;
    if owner.is_some_and(|owner| owner != ctx.device_id.to_string()) {
        return Err(invalid());
    }
    let mut keys = Vec::with_capacity(incoming.len());
    let mut contacts = Vec::with_capacity(incoming.len());
    for source in incoming {
        let (source_key, contact) = platform_contact(&tx, &book_id, source)?;
        keys.push(source_key);
        contacts.push(contact);
    }
    let captured = contacts::capture_tx(&tx, ctx, book, &contacts)?;
    tx.commit()?;
    let out = keys
        .into_iter()
        .zip(&captured)
        .map(|(source_key, (id, revision, changed))| {
            json!({"source_key": source_key, "id": id, "revision": revision, "changed": changed})
        })
        .collect::<Vec<_>>();
    Ok(json!({"schema_version": 1, "status": "captured", "book_id": book_id, "contacts": out, "count": incoming.len()}).to_string())
}

pub(crate) fn context(conn: &Connection, device_id: &str, input: &str) -> Result<String, Error> {
    let input = parse(input)?;
    schema(&input)?;
    object_keys(
        &input,
        &["schema_version", "book_id", "contact_id", "source_key"],
    )?;
    let book_id = req(&input, "book_id")?;
    // Exactly one of `contact_id` (core id) or `source_key` (provider key).
    let by_contact = input.get("contact_id").is_some();
    if by_contact == input.get("source_key").is_some() {
        return Err(invalid());
    }
    let owner: String = conn
        .query_row(
            "SELECT owner_device_id FROM contact_books WHERE id=? AND forgotten=0",
            [book_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    if owner != device_id {
        return Err(Error::InvalidRequest("not contact book owner"));
    }
    let contact_id = if by_contact {
        req(&input, "contact_id")?.to_owned()
    } else {
        conn.query_row(
            "SELECT contact_id FROM contact_source_map WHERE book_id=? AND source_key=?",
            params![book_id, req(&input, "source_key")?],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(Error::NotFound)?
    };
    let contact_id = contact_id.as_str();
    let source = source_for(conn, book_id, Some(contact_id))?;
    let source_key = source
        .get("source_key")
        .filter(|k| k.is_string())
        .ok_or(Error::NotFound)?
        .clone();
    let body: String = conn
        .query_row(
            "SELECT body FROM contacts WHERE book_id=? AND id=? AND deleted_at IS NULL",
            params![book_id, contact_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    let mut observed: Value = serde_json::from_str(&body).map_err(|_| Error::Database)?;
    // Native sees provider item ids in place of core field ids.
    for map in source["field_sources"].as_array().into_iter().flatten() {
        let (Some(field), Some(id)) = (map["field"].as_str(), map["id"].as_str()) else {
            continue;
        };
        for item in observed
            .get_mut(field)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
            .filter(|item| item.get("id").and_then(Value::as_str) == Some(id))
        {
            item["id"] = map["source_id"].clone();
        }
    }
    if let Some(obj) = observed.as_object_mut() {
        obj.remove("digest");
    }
    Ok(json!({
        "schema_version": 1,
        "contact_id": contact_id,
        "source_key": source_key,
        "field_sources": source["field_sources"],
        "provenance": source["provenance"],
        "observed": observed,
    })
    .to_string())
}
