//! Query and settings APIs for contact books, requests, and restore operations.
//!
//! These APIs provide:
//! - `list_contact_books_json()` → sanitized books with state/capability/policy
//! - `contact_settings_json(input)` → owner-only configuration read/write
//! - `list_contact_requests_json(input)` → pending requests filtered by owner/requester
//! - `contact_approval_json(request_id, approve)` → approve/reject local-only
//! - `list_restorable_contacts_json(book_id)` → 90-day tombstones
//! - `restore_contact_json(book_id, id)` → enqueue create request with restored fields
//!
//! Results are paginated (≤200 items) and sanitized to remove provider IDs.

use crate::{Ctx, Error};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const RESULT_LIMIT: i64 = 200;
const TOMBSTONE_TTL_SECONDS: i64 = 90 * 24 * 60 * 60; // 90 days

fn invalid() -> Error {
    Error::InvalidRequest("invalid contact request")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn req<'a>(v: &'a Value, key: &str) -> Result<&'a str, Error> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 1024)
        .ok_or_else(invalid)
}

fn sanitize(mut value: Value) -> Value {
    crate::contacts::strip_sources(&mut value);
    value
}

/// Returns all non-forgotten contact books (both owned and synced from others).
/// Includes owner_device_id for UI routing. Desktop/Android can view/sync books
/// from other devices but settings/approval restricted to owner.
///
/// JSON response: `{ "books": [{ "id", "state", "owner_device_id", "capabilities",
/// "accounts", "policy", "effective_remote_edits", "inventory", "contact_count" }],
/// "schema_version": 1 }`. Provider ids (including `default_account_id`) are never present.
pub(crate) fn list_books(conn: &Connection) -> Result<String, Error> {
    let mut stmt = conn.prepare("SELECT body FROM contact_books WHERE forgotten=0 ORDER BY id")?;

    let books = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(Result::ok)
        .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
        .take(RESULT_LIMIT as usize)
        .map(|mut book| {
            book = sanitize(book);
            if let Some(obj) = book.as_object_mut() {
                // Count live (non-deleted) contacts in this book
                let book_id = obj.get("id").and_then(Value::as_str).unwrap_or("");
                let count = conn
                    .query_row(
                        "SELECT COUNT(*) FROM contacts WHERE book_id=? AND deleted_at IS NULL",
                        [book_id],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap_or(0);
                obj.insert("contact_count".to_string(), json!(count));
            }
            let effective = effective_remote_edits(&book);
            book["effective_remote_edits"] = json!(effective);
            book
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "schema_version": 1,
        "books": books,
    })
    .to_string())
}

/// The mode core enforces: an absent policy means the default (`auto`, delete-capped).
fn effective_remote_edits(book: &Value) -> &str {
    book.pointer("/policy/remote_edits")
        .and_then(Value::as_str)
        .unwrap_or(crate::contacts::DEFAULT_REMOTE_EDITS)
}

/// Owner-only book settings. `{book_id}` reads; adding `policy` and/or
/// `default_account_id` updates. Native-only: the response includes the raw source book
/// (provider accounts), which never appears in synced or UI book DTOs.
///
/// Response: `{ "schema_version", "book_id", "status": "current"|"updated", "policy",
/// "effective_remote_edits", "policy_source", "default_account_id", "accounts",
/// "source_book" }`
pub(crate) fn configure_settings(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &str,
) -> Result<String, Error> {
    let input: Value = serde_json::from_str(input).map_err(|_| invalid())?;
    let obj = input.as_object().ok_or_else(invalid)?;
    if !obj.keys().all(|k| {
        matches!(
            k.as_str(),
            "schema_version" | "book_id" | "policy" | "default_account_id"
        )
    }) || obj
        .get("schema_version")
        .is_some_and(|v| v.as_u64() != Some(1))
    {
        return Err(invalid());
    }
    let book_id = req(&input, "book_id")?;
    let policy = input.get("policy");
    let account = input.get("default_account_id");
    let updating = policy.is_some() || account.is_some();
    let tx = conn.transaction()?;
    let (mut body, _) = crate::contacts::owned_book(&tx, ctx, book_id)?;
    let mut raw = crate::contacts::book_source(&tx, book_id)?;
    if updating {
        crate::contacts::require_unlocked(ctx)?;
        if let Some(policy) = policy {
            let policy = policy.as_object().ok_or_else(invalid)?;
            for (key, value) in policy {
                let ok = match key.as_str() {
                    "remote_edits" => {
                        matches!(value.as_str(), Some("auto" | "confirm" | "off"))
                    }
                    // Mandatory safety: the cap cannot be waived.
                    "large_delete_requires_approval" => value.as_bool() == Some(true),
                    _ => false,
                };
                if !ok {
                    return Err(invalid());
                }
            }
            let target = body
                .as_object_mut()
                .ok_or(Error::Database)?
                .entry("policy")
                .or_insert_with(|| json!({}));
            if !target.is_object() {
                *target = json!({});
            }
            for (key, value) in policy {
                target[key.as_str()] = value.clone();
            }
        }
        if let Some(account) = account {
            let account = account
                .as_str()
                .filter(|a| !a.is_empty() && a.len() <= 1024)
                .ok_or_else(invalid)?;
            let listed = raw
                .get("accounts")
                .and_then(Value::as_array)
                .is_some_and(|accounts| {
                    accounts.iter().any(|a| {
                        a.get("id").and_then(Value::as_str) == Some(account)
                            && a.get("writable").and_then(Value::as_bool) != Some(false)
                    })
                });
            if !listed {
                return Err(Error::InvalidRequest(
                    "unknown or read-only contact account",
                ));
            }
            raw["default_account_id"] = json!(account);
            tx.execute(
                "UPDATE contact_book_sources SET body=? WHERE book_id=?",
                params![
                    serde_json::to_string(&raw).map_err(|_| Error::Database)?,
                    book_id
                ],
            )?;
        }
        body["last_config_at"] = json!(now());
        tx.execute(
            "UPDATE contact_books SET body=? WHERE id=?",
            params![
                serde_json::to_string(&body).map_err(|_| Error::Database)?,
                book_id
            ],
        )?;
        // Peers receive only the sanitized book state (no account ids).
        crate::contacts::publish_book_state(&tx, ctx, book_id)?;
    }
    tx.commit()?;
    let policy_source = if body.get("last_config_at").is_some() {
        "owner_settings"
    } else if body.pointer("/policy/remote_edits").is_some() {
        "capture"
    } else {
        "default"
    };
    Ok(json!({
        "schema_version": 1,
        "book_id": book_id,
        "status": if updating { "updated" } else { "current" },
        "policy": body.get("policy").cloned().unwrap_or(json!({})),
        "effective_remote_edits": effective_remote_edits(&body),
        "policy_source": policy_source,
        "default_account_id": raw.get("default_account_id").cloned().unwrap_or(Value::Null),
        "accounts": raw.get("accounts").cloned().unwrap_or(json!([])),
        "source_book": raw,
    })
    .to_string())
}

/// List contact edit requests plus durable scan-deletion holds.
///
/// Input: `{ "book_id"?, "requester"?, "limit"? (<=200) }`
/// - `book_id`: requests targeting that book (owner only) and its holds
/// - `requester`: requests from that device (no holds)
/// - neither: all ledger rows and this owner's holds
///
/// `applying`/`outcome_unknown` recovery rows sort first so they are never cut off.
/// Each request carries a sanitized summary (`kind`, `contact_id`, `display_name`,
/// `field_paths`); holds are `{kind:"scan_deletions", scan_id, book_id, state, count,
/// sample_names, created_at}`. No patch values or provider identity are returned.
pub(crate) fn list_requests(
    conn: &Connection,
    device_id: &str,
    input: &str,
) -> Result<String, Error> {
    let req_json: Value = serde_json::from_str(input).map_err(|_| invalid())?;
    let book_filter = req_json.get("book_id").and_then(Value::as_str);
    let requester_filter = req_json.get("requester").and_then(Value::as_str);
    let limit = req_json
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(RESULT_LIMIT)
        .clamp(1, RESULT_LIMIT);

    if let Some(book_id) = book_filter {
        let owner: String = conn
            .query_row(
                "SELECT owner_device_id FROM contact_books WHERE id=? AND forgotten=0",
                [book_id],
                |r| r.get(0),
            )
            .map_err(|_| Error::NotFound)?;
        if owner != device_id {
            return Err(Error::InvalidRequest("not book owner"));
        }
    }

    let rows = conn
        .prepare(
            "SELECT request_id,status,book_id,requester,expires_at,observed,body FROM contact_edit_ledger \
             WHERE (?1 IS NULL OR requester=?1) AND (?2 IS NULL OR book_id=?2) \
             ORDER BY CASE WHEN status IN ('applying','outcome_unknown') THEN 0 ELSE 1 END, expires_at DESC, request_id \
             LIMIT ?3",
        )?
        .query_map(params![requester_filter, book_filter, limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut requests = Vec::with_capacity(rows.len());
    for (request_id, status, book_id, requester, expires_at, observed, body) in rows {
        let body: Value = serde_json::from_str(&body).map_err(|_| Error::Database)?;
        let mut item = crate::contacts::request_summary(conn, &book_id, &body)?;
        item["request_id"] = json!(request_id);
        item["state"] = json!(status);
        item["book_id"] = json!(book_id);
        item["requester"] = json!(requester);
        item["expires_at"] = json!(expires_at);
        if let Some(observed) = observed.and_then(|o| serde_json::from_str::<Value>(&o).ok()) {
            item["observed"] = sanitize(observed);
        }
        requests.push(item);
    }
    if requester_filter.is_none() {
        requests.extend(crate::contacts::pending_holds(
            conn,
            device_id,
            book_filter,
            limit,
        )?);
    }

    Ok(json!({
        "schema_version": 1,
        "requests": requests,
    })
    .to_string())
}

/// Approve or reject an edit request (`request_id`) or a held scan deletion (`scan_id`).
///
/// Input: `{ "request_id" | "scan_id", "approve": true|false }`. The decision, its
/// owner-authored result and the scan-hold execution live in `contacts::decide_approval`.
pub(crate) fn decide_approval(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &str,
) -> Result<String, Error> {
    let req_json: Value = serde_json::from_str(input).map_err(|_| invalid())?;
    crate::contacts::decide_approval(conn, ctx, &req_json)
}

/// List contacts deleted in the past 90 days (within retention window).
///
/// Input: `{ "book_id" }`
/// Response: `{ "contacts": [{ "id", "display_name", "deleted_at", "photo"? }], ... }`
pub(crate) fn list_restorable(conn: &Connection, input: &str) -> Result<String, Error> {
    let req_json: Value = serde_json::from_str(input).map_err(|_| invalid())?;
    let book_id = req(&req_json, "book_id")?;

    // Verify book exists (forgotten=0)
    let _ = conn
        .query_row(
            "SELECT id FROM contact_books WHERE id=? AND forgotten=0",
            [book_id],
            |r| r.get::<_, String>(0),
        )
        .map_err(|_| Error::NotFound)?;

    let cutoff = now() - TOMBSTONE_TTL_SECONDS;

    let mut stmt = conn.prepare(
        "SELECT id,body,deleted_at FROM contacts \
         WHERE book_id=? AND deleted_at IS NOT NULL AND deleted_at > ? \
           AND json_type(body,'$.restored_to') IS NULL \
         ORDER BY deleted_at DESC LIMIT ?",
    )?;

    let contacts = stmt
        .query_map(params![book_id, cutoff, RESULT_LIMIT], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .filter_map(Result::ok)
        .filter_map(|(id, body, deleted_at)| {
            serde_json::from_str::<Value>(&body)
                .ok()
                .map(|mut contact| {
                    contact = sanitize(contact);
                    let mut obj = json!({
                        "id": id,
                        "deleted_at": deleted_at,
                    });
                    if let Some(name) = contact.get("display_name") {
                        obj["display_name"] = name.clone();
                    }
                    // Reference only ({attachment_id, available}); never descriptor or key.
                    if let Some(photo) = crate::contact_media::present(conn, contact).get("photo") {
                        obj["photo"] = photo.clone();
                    }
                    obj
                })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "schema_version": 1,
        "book_id": book_id,
        "contacts": contacts,
    })
    .to_string())
}

/// Restore a deleted contact by enqueueing a new "create" edit request with preserved fields.
///
/// Input: `{ "book_id", "contact_id" }`
/// Enqueues a new ContactEditRequest with:
/// - `kind: "create"`
/// - Restored fields from tombstone
/// - Photo reference from tombstone (if present)
/// - New request UUID and a `restored_from` link to the retained tombstone
///
/// Never writes to OS; enqueue only.
pub(crate) fn restore_contact(
    conn: &mut Connection,
    ctx: &Ctx<'_>,
    input: &str,
) -> Result<String, Error> {
    let req_json: Value = serde_json::from_str(input).map_err(|_| invalid())?;
    let book_id = req(&req_json, "book_id")?;
    let contact_id = req(&req_json, "contact_id")?;

    // Any device holding the retained tombstone may ask the book owner to recreate it;
    // only the owner's permit/reconcile path ever affects the OS.
    let owner: String = conn
        .query_row(
            "SELECT owner_device_id FROM contact_books WHERE id=? AND forgotten=0",
            [book_id],
            |r| r.get(0),
        )
        .map_err(|_| Error::NotFound)?;

    let (tombstone_body, deleted_at): (String, i64) = conn
        .query_row(
            "SELECT body,deleted_at FROM contacts WHERE book_id=? AND id=? AND deleted_at IS NOT NULL",
            params![book_id, contact_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| Error::NotFound)?;
    if now() - deleted_at > TOMBSTONE_TTL_SECONDS {
        return Err(Error::InvalidRequest("restore window expired"));
    }
    let restored_fields: Value =
        sanitize(serde_json::from_str(&tombstone_body).map_err(|_| Error::Database)?);
    if restored_fields.get("restored_to").is_some() {
        return Err(Error::InvalidRequest("contact already restored"));
    }
    // Requester-local dedupe: reuse this device's own still-open restore of the tombstone.
    // (Only the owner's durable claim is authoritative across requesters.)
    let mine = conn
        .prepare("SELECT request_id,body FROM contact_edit_ledger WHERE book_id=? AND requester=? AND status NOT IN ('rejected','expired','failed','conflict')")?
        .query_map(params![book_id, ctx.device_id.to_string()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (request_id, body) in mine {
        let body: Value = serde_json::from_str(&body).map_err(|_| Error::Database)?;
        if body
            .pointer("/provenance/restored_from")
            .and_then(Value::as_str)
            == Some(contact_id)
        {
            return Ok(json!({
                "schema_version": 1,
                "request_id": request_id,
                "status": "enqueued",
                "duplicate": true,
                "book_id": book_id,
                "contact_id": contact_id,
                "target_owner": owner,
            })
            .to_string());
        }
    }
    let new_request_id = format!("restore-{}", Uuid::new_v4());

    let mut restore_request = json!({
        "schema_version": 1,
        "request_id": new_request_id,
        "target_owner": owner,
        "book_id": book_id,
        "kind": "create",
        "provenance": {
            "restored_from": contact_id,
            "restored_at": now(),
        },
        "expires_at": now() + (7 * 24 * 60 * 60), // 7 days
    });

    // Include restored contact fields in the request
    if let Some(req_obj) = restore_request.as_object_mut() {
        for field in [
            "display_name",
            "name",
            "phones",
            "emails",
            "addresses",
            "notes",
            "photo",
            "nickname",
            "organization",
            "title",
            "birthday",
        ] {
            if let Some(val) = restored_fields.get(field).filter(|v| !v.is_null()) {
                req_obj.insert(field.to_string(), val.clone());
            }
        }
        // Add photo_op if photo present
        if let Some(photo) = restored_fields.get("photo").filter(|v| !v.is_null()) {
            req_obj.insert(
                "photo_op".to_string(),
                json!({"op": "set", "attachment_id": photo}),
            );
        }
    }

    let local_owner = owner == ctx.device_id.to_string();
    crate::contacts::submit_request(conn, ctx, restore_request, local_owner)?;

    Ok(json!({
        "schema_version": 1,
        "request_id": new_request_id,
        "status": "enqueued",
        "book_id": book_id,
        "contact_id": contact_id,
        "target_owner": owner,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_test_db() -> (Connection, String, String) {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        crate::contacts::initialize(&conn).expect("init contacts");
        conn.execute_batch("BEGIN EXCLUSIVE").expect("begin tx");
        let device_id = "device-001".to_string();
        let book_id = "book-001".to_string();
        (conn, device_id, book_id)
    }

    fn insert_test_book(
        conn: &Connection,
        book_id: &str,
        owner_id: &str,
        forgotten: bool,
    ) -> Result<(), Error> {
        let book = json!({
            "id": book_id,
            "owner_device_id": owner_id,
            "generation": "1",
            "state": "active",
            "capabilities": {
                "read": true,
                "write": true,
                "photo": true,
            },
            "accounts": [{
                "id": "native:test",
                "name": "Test Account",
                "writable": true,
            }],
            "default_account_id": "native:test",
            "policy": {
                "remote_edits": "confirm",
                "large_delete_requires_approval": true,
            }
        });
        conn.execute(
            "INSERT INTO contact_books(id,owner_device_id,generation,state,body,forgotten) VALUES(?,?,?,?,?,?)",
            params![book_id, owner_id, 1i64, "active", serde_json::to_string(&book).unwrap(), if forgotten { 1 } else { 0 }],
        ).map_err(|_| Error::Database)?;
        Ok(())
    }

    fn insert_test_contact(
        conn: &Connection,
        contact_id: &str,
        book_id: &str,
        display_name: &str,
        deleted_at: Option<i64>,
    ) -> Result<(), Error> {
        let contact = json!({
            "id": contact_id,
            "book_id": book_id,
            "revision": "1",
            "display_name": display_name,
            "name": { "given": "Test", "family": "User" },
            "phones": [],
            "emails": [],
            "addresses": [],
        });
        conn.execute(
            "INSERT INTO contacts(id,book_id,revision,body,deleted_at) VALUES(?,?,?,?,?)",
            params![
                contact_id,
                book_id,
                1i64,
                serde_json::to_string(&contact).unwrap(),
                deleted_at
            ],
        )
        .map_err(|_| Error::Database)?;
        Ok(())
    }

    #[test]
    fn test_list_books_basic() {
        let (conn, device_id, book_id) = setup_test_db();

        insert_test_book(&conn, &book_id, &device_id, false).expect("insert book");

        let result = list_books(&conn).expect("list_books");
        let parsed: Value = serde_json::from_str(&result).expect("parse json");

        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["books"].as_array().expect("array").len(), 1);
        assert_eq!(parsed["books"][0]["id"], book_id);
        assert_eq!(parsed["books"][0]["contact_count"], 0);
    }

    #[test]
    fn test_list_books_excludes_forgotten() {
        let (conn, device_id, book_id) = setup_test_db();

        insert_test_book(&conn, &book_id, &device_id, true).expect("insert forgotten book");

        let result = list_books(&conn).expect("list_books");
        let parsed: Value = serde_json::from_str(&result).expect("parse json");

        assert_eq!(parsed["books"].as_array().expect("array").len(), 0);
    }

    #[test]
    fn test_list_restorable_within_90_days() {
        let (conn, device_id, book_id) = setup_test_db();

        insert_test_book(&conn, &book_id, &device_id, false).expect("insert book");
        let deleted_at = now() - (30 * 24 * 60 * 60); // 30 days ago
        insert_test_contact(
            &conn,
            "contact-1",
            &book_id,
            "Deleted User",
            Some(deleted_at),
        )
        .expect("insert deleted contact");

        let input = json!({ "book_id": book_id }).to_string();
        let result = list_restorable(&conn, &input).expect("list_restorable");
        let parsed: Value = serde_json::from_str(&result).expect("parse json");

        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["book_id"], book_id);
        assert_eq!(parsed["contacts"].as_array().expect("array").len(), 1);
        assert_eq!(parsed["contacts"][0]["id"], "contact-1");
        assert_eq!(parsed["contacts"][0]["display_name"], "Deleted User");
    }

    #[test]
    fn restored_tombstones_do_not_hide_older_restorable_contacts() {
        let (conn, device_id, book_id) = setup_test_db();
        insert_test_book(&conn, &book_id, &device_id, false).unwrap();
        let deleted_at = now() - 60;
        for index in 0..RESULT_LIMIT {
            let id = format!("restored-{index}");
            insert_test_contact(&conn, &id, &book_id, "Restored", Some(deleted_at)).unwrap();
            conn.execute(
                "UPDATE contacts SET body=json_set(body,'$.restored_to','new-contact') WHERE book_id=? AND id=?",
                params![book_id, id],
            )
            .unwrap();
        }
        insert_test_contact(
            &conn,
            "still-restorable",
            &book_id,
            "Older deletion",
            Some(deleted_at - 1),
        )
        .unwrap();

        let result: Value = serde_json::from_str(
            &list_restorable(&conn, &json!({"book_id": book_id}).to_string()).unwrap(),
        )
        .unwrap();
        assert_eq!(result["contacts"].as_array().unwrap().len(), 1);
        assert_eq!(result["contacts"][0]["id"], "still-restorable");
    }

    #[test]
    fn test_list_restorable_excludes_expired() {
        let (conn, device_id, book_id) = setup_test_db();

        insert_test_book(&conn, &book_id, &device_id, false).expect("insert book");
        let deleted_at = now() - (95 * 24 * 60 * 60); // 95 days ago (expired)
        insert_test_contact(
            &conn,
            "contact-1",
            &book_id,
            "Old Contact",
            Some(deleted_at),
        )
        .expect("insert expired contact");

        let input = json!({ "book_id": book_id }).to_string();
        let result = list_restorable(&conn, &input).expect("list_restorable");
        let parsed: Value = serde_json::from_str(&result).expect("parse json");

        assert_eq!(parsed["contacts"].as_array().expect("array").len(), 0);
    }

    #[test]
    fn test_list_restorable_not_found_for_nonexistent_book() {
        let (conn, _, _) = setup_test_db();

        let input = json!({ "book_id": "nonexistent" }).to_string();
        let result = list_restorable(&conn, &input);

        assert!(matches!(result, Err(Error::NotFound)));
    }

    #[test]
    fn test_list_books_returns_all_synced() {
        let (conn, device_id, book_id) = setup_test_db();
        let other_device = "device-999";

        // Insert books from different owners
        insert_test_book(&conn, &book_id, &device_id, false).expect("insert owned");
        insert_test_book(&conn, "book-999", other_device, false).expect("insert synced from other");

        // list_books should return ALL non-forgotten books (not owner-filtered)
        let result = list_books(&conn).expect("list_books");
        let parsed: Value = serde_json::from_str(&result).expect("parse json");

        let books = parsed["books"].as_array().expect("array");
        assert_eq!(books.len(), 2, "Should list all non-forgotten books");

        // Verify both books are present with owner_device_id for UI routing
        let ids: Vec<&str> = books
            .iter()
            .filter_map(|b| b.get("id").and_then(|id| id.as_str()))
            .collect();
        assert!(ids.contains(&book_id.as_str()));
        assert!(ids.contains(&"book-999"));

        // Verify owner_device_id is included for routing
        for book in books {
            assert!(
                book.get("owner_device_id").is_some(),
                "owner_device_id must be present"
            );
        }
    }

    #[test]
    fn test_list_requests_includes_crash_reconciliation_rows() {
        let (conn, device_id, book_id) = setup_test_db();

        insert_test_book(&conn, &book_id, &device_id, false).expect("insert book");

        // Insert requests in various states including crash reconciliation states
        conn.execute(
            "INSERT INTO contact_edit_ledger(request_id,book_id,requester,body,status,expires_at) VALUES(?,?,?,?,?,?)",
            params!["req-1", &book_id, "requester-1", "{}", "requested", now() + 3600],
        ).expect("insert requested");

        conn.execute(
            "INSERT INTO contact_edit_ledger(request_id,book_id,requester,body,status,expires_at) VALUES(?,?,?,?,?,?)",
            params!["req-2", &book_id, "requester-1", "{}", "approved", now() + 3600],
        ).expect("insert approved");

        conn.execute(
            "INSERT INTO contact_edit_ledger(request_id,book_id,requester,body,status,expires_at) VALUES(?,?,?,?,?,?)",
            params!["req-3", &book_id, "requester-1", "{}", "applying", now() + 3600],
        ).expect("insert applying (crash state)");

        conn.execute(
            "INSERT INTO contact_edit_ledger(request_id,book_id,requester,body,status,expires_at) VALUES(?,?,?,?,?,?)",
            params!["req-4", &book_id, "requester-1", "{}", "outcome_unknown", now() + 3600],
        ).expect("insert outcome_unknown (crash state)");

        let input = json!({}).to_string();
        let result = list_requests(&conn, &device_id, &input).expect("list_requests");
        let parsed: Value = serde_json::from_str(&result).expect("parse json");

        let requests = parsed["requests"].as_array().expect("array");
        assert_eq!(
            requests.len(),
            4,
            "Should include all rows including crash states"
        );

        // Verify all states are present
        let states: Vec<&str> = requests
            .iter()
            .filter_map(|r| r.get("state").and_then(|s| s.as_str()))
            .collect();
        assert!(states.contains(&"requested"));
        assert!(states.contains(&"approved"));
        assert!(states.contains(&"applying"));
        assert!(states.contains(&"outcome_unknown"));
    }

    #[test]
    fn test_list_restorable_limits_results() {
        let (conn, device_id, book_id) = setup_test_db();

        insert_test_book(&conn, &book_id, &device_id, false).expect("insert book");

        // Insert many deleted contacts
        for i in 0..300 {
            let contact_id = format!("contact-{}", i);
            let deleted_at = now() - ((30 + (i % 30)) * 24 * 60 * 60);
            insert_test_contact(
                &conn,
                &contact_id,
                &book_id,
                &format!("User {}", i),
                Some(deleted_at),
            )
            .expect("insert contact");
        }

        let input = json!({ "book_id": book_id }).to_string();
        let result = list_restorable(&conn, &input).expect("list_restorable");
        let parsed: Value = serde_json::from_str(&result).expect("parse json");

        let contacts = parsed["contacts"].as_array().expect("array");
        assert!(contacts.len() <= RESULT_LIMIT as usize);
        assert_eq!(contacts.len(), RESULT_LIMIT as usize); // Should be exactly 200
    }

    #[test]
    fn test_list_books_includes_contact_count() {
        let (conn, device_id, book_id) = setup_test_db();

        insert_test_book(&conn, &book_id, &device_id, false).expect("insert book");
        insert_test_contact(&conn, "contact-1", &book_id, "User 1", None).expect("insert");
        insert_test_contact(&conn, "contact-2", &book_id, "User 2", None).expect("insert");
        insert_test_contact(&conn, "contact-3", &book_id, "Deleted User", Some(now()))
            .expect("insert deleted");

        let result = list_books(&conn).expect("list_books");
        let parsed: Value = serde_json::from_str(&result).expect("parse json");

        // Should count only non-deleted
        assert_eq!(parsed["books"][0]["contact_count"], 2);
    }

    #[test]
    fn test_list_books_sanitizes_provenance() {
        let (conn, device_id, book_id) = setup_test_db();

        let book = json!({
            "id": book_id,
            "owner_device_id": device_id,
            "generation": "1",
            "state": "active",
            "provenance": { "source_id": "secret", "account_id": "secret" },
            "policy": { "remote_edits": "confirm" }
        });
        conn.execute(
            "INSERT INTO contact_books(id,owner_device_id,generation,state,body,forgotten) VALUES(?,?,?,?,?,0)",
            params![&book_id, &device_id, 1i64, "active", serde_json::to_string(&book).unwrap()],
        ).expect("insert");

        let result = list_books(&conn).expect("list_books");
        let parsed: Value = serde_json::from_str(&result).expect("parse json");

        let returned_book = &parsed["books"][0];
        assert!(
            !returned_book
                .as_object()
                .unwrap()
                .contains_key("provenance")
        );
        assert!(!returned_book.as_object().unwrap().contains_key("source_id"));
        assert!(
            !returned_book
                .as_object()
                .unwrap()
                .contains_key("account_id")
        );
    }
}
