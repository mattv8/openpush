//! Bounded recipient discovery over live contact phone numbers.
//!
//! Display-only: rows name a contact and one of its phone numbers so a host can offer the
//! number as a message recipient. The recipient is always the phone address, never the
//! contact ID. Matching uses only displayable values (names, nickname, organization and phone
//! digits), never serialized JSON keys, and numbers are normalized with the book's region; a
//! number that cannot be normalized is returned as-is and marked `normalized:false` rather
//! than turned into an invented E.164 value.
use crate::{Error, contact_resolution};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::HashMap;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;
const MAX_QUERY_CHARS: usize = 128;
/// Rows gathered before ranking; bounds work on very broad queries.
const MAX_CANDIDATES: usize = 500;

fn invalid() -> Error {
    Error::InvalidRequest("invalid contact search request")
}

fn digits(value: &str) -> String {
    value.chars().filter(char::is_ascii_digit).collect()
}

/// Lowercased displayable name text of a contact.
fn names(contact: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["display_name", "nickname", "organization"] {
        if let Some(text) = contact.get(key).and_then(Value::as_str) {
            out.push(text.to_lowercase());
        }
    }
    if let Some(name) = contact.get("name").and_then(Value::as_object) {
        out.extend(
            name.values()
                .filter_map(Value::as_str)
                .map(str::to_lowercase),
        );
    }
    out
}

/// `{schema_version:1, query, limit?, source_device_id?}` → `{results:[{contact_id, book_id,
/// display_name, phone_id, label?, value, address, normalized, photo_attachment_id?}]}`.
pub(crate) fn search(conn: &Connection, input: &str) -> Result<String, Error> {
    let input: Value = serde_json::from_str(input).map_err(|_| invalid())?;
    if input.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err(invalid());
    }
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty() && q.chars().count() <= MAX_QUERY_CHARS)
        .ok_or_else(invalid)?
        .to_lowercase();
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_LIMIT, |l| (l as usize).clamp(1, MAX_LIMIT));
    let source = input.get("source_device_id").and_then(Value::as_str);
    let query_digits = digits(&query);
    let phone_query = query_digits.len() >= 2
        && query
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '(' | ')' | '.' | '-'));

    let mut books = HashMap::new();
    for row in conn
        .prepare("SELECT id,owner_device_id,body FROM contact_books WHERE forgotten=0")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
    {
        let (id, owner, body) = row?;
        books.insert(id, (owner, contact_resolution::region(&body)));
    }

    let mut rows = Vec::new();
    let mut statement = conn.prepare(
        "SELECT c.book_id,c.id,c.body FROM contacts c JOIN contact_books b ON b.id=c.book_id AND b.forgotten=0 \
         WHERE c.deleted_at IS NULL ORDER BY c.book_id,c.id",
    )?;
    let mut scan = statement.query([])?;
    while let Some(row) = scan.next()? {
        if rows.len() >= MAX_CANDIDATES {
            break;
        }
        let (book_id, contact_id, body): (String, String, String) =
            (row.get(0)?, row.get(1)?, row.get(2)?);
        // Cheap prefilter on the raw text; the real match below only reads displayable values.
        if !phone_query && !body.to_lowercase().contains(&query) {
            continue;
        }
        let Ok(contact) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        let names = names(&contact);
        let name_rank = if names.iter().any(|n| n.starts_with(&query)) {
            Some(0)
        } else if names.iter().any(|n| n.contains(&query)) {
            Some(1)
        } else {
            None
        };
        let Some((owner, region)) = books.get(&book_id) else {
            continue;
        };
        let display_name = contact
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let photo = contact
            .get("photo")
            .and_then(|p| p.get("attachment_id").or(Some(p)))
            .and_then(Value::as_str)
            .map(str::to_owned);
        for phone in contact
            .get("phones")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(phone_id), Some(value)) = (
                phone.get("id").and_then(Value::as_str),
                phone.get("value").and_then(Value::as_str),
            ) else {
                continue;
            };
            let phone_match = phone_query && digits(value).contains(&query_digits);
            let rank = match (name_rank, phone_match) {
                (Some(rank), _) => rank,
                (None, true) => 2,
                (None, false) => continue,
            };
            let e164 = contact_resolution::normalized(value, region.as_deref());
            let address = match &e164 {
                Some(e164) => e164.clone(),
                None => {
                    let raw = digits(value);
                    if !(3..=15).contains(&raw.len()) {
                        continue;
                    }
                    raw
                }
            };
            let mut item = json!({
                "contact_id": contact_id,
                "book_id": book_id,
                "display_name": display_name,
                "phone_id": phone_id,
                "value": value,
                "address": address,
                "normalized": e164.is_some(),
            });
            if let Some(label) = phone.get("label").and_then(Value::as_str) {
                item["label"] = json!(label);
            }
            if let Some(photo) = &photo {
                item["photo_attachment_id"] = json!(photo);
            }
            // Same-source books sort first when the host knows the route's phone.
            let source_rank = usize::from(source.is_some_and(|s| s != owner));
            rows.push(((rank, source_rank, display_name.to_lowercase()), item));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let results = rows
        .into_iter()
        .take(limit)
        .map(|(_, item)| item)
        .collect::<Vec<_>>();
    Ok(json!({"schema_version": 1, "results": results}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::contacts::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO contact_books(id,owner_device_id,generation,state,body,forgotten) VALUES('b1','phone',1,'active',?,0)",
            [json!({"id":"b1","region":"US"}).to_string()],
        )
        .unwrap();
        let insert = |id: &str, body: Value| {
            conn.execute(
                "INSERT INTO contacts(id,book_id,revision,body,deleted_at) VALUES(?,'b1',1,?,NULL)",
                rusqlite::params![id, body.to_string()],
            )
            .unwrap();
        };
        insert(
            "c1",
            json!({"id":"c1","display_name":"Ada Lovelace","nickname":"Countess","phones":[{"id":"p1","label":"mobile","value":"(202) 555-0100"},{"id":"p2","label":"work","value":"+44 20 7946 0000"}]}),
        );
        insert(
            "c2",
            json!({"id":"c2","display_name":"Grace Hopper","phones":[{"id":"p3","value":"12345"}]}),
        );
        insert(
            "c3",
            json!({"id":"c3","display_name":"No Phone","nickname":"Nick"}),
        );
        conn
    }

    fn run(conn: &Connection, query: &str) -> Vec<Value> {
        let out: Value = serde_json::from_str(
            &search(conn, &json!({"schema_version":1,"query":query}).to_string()).unwrap(),
        )
        .unwrap();
        out["results"].as_array().unwrap().clone()
    }

    #[test]
    fn names_return_every_phone_with_normalized_addresses() {
        let conn = db();
        let rows = run(&conn, "ada");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["address"], "+12025550100");
        assert_eq!(rows[1]["address"], "+442079460000");
        assert!(
            rows.iter()
                .all(|r| r["contact_id"] == "c1" && r["normalized"] == true)
        );
    }

    #[test]
    fn keys_never_match_and_short_codes_are_not_invented_e164() {
        let conn = db();
        // "nick" appears as a JSON key ("nickname") in every row with a nickname; only c3's
        // value matches, and c3 has no phone, so nothing is offered.
        assert!(run(&conn, "nick").is_empty());
        assert!(run(&conn, "phones").is_empty());
        let rows = run(&conn, "12345");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["address"], "12345");
        assert_eq!(rows[0]["normalized"], false);
        let rows = run(&conn, "555-01");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["phone_id"], "p1");
    }

    #[test]
    fn invalid_queries_fail_closed() {
        let conn = db();
        assert!(search(&conn, &json!({"schema_version":1,"query":"  "}).to_string()).is_err());
        assert!(search(&conn, &json!({"query":"ada"}).to_string()).is_err());
    }
}
