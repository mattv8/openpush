//! Address resolution uses real captured contact projections, never native IDs.

mod common;
use common::*;
use serde_json::{Value, json};
use tempfile::TempDir;

fn capture(
    client: &Client,
    owner: &ClientConfig,
    book_id: &str,
    region: &str,
    generation: &str,
    contacts: Value,
) {
    client.capture_contact_book(&json!({
        "schema_version": 1,
        "book": {"id": book_id, "owner_device_id": owner.device_id.to_string(), "generation": generation, "state": "active", "region": region},
        "contacts": contacts,
    }).to_string()).unwrap();
}

fn contact(book_id: &str, id: &str, name: &str, phone: &str) -> Value {
    json!({"id":id,"book_id":book_id,"display_name":name,"name":{},"phones":[{"id":"p1","value":phone}],"emails":[],"addresses":[]})
}

fn resolve(client: &Client, addresses: &[&str]) -> Value {
    serde_json::from_str(
        &client
            .resolve_contact_addresses_json(&json!({"addresses": addresses}).to_string())
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn resolves_international_and_national_per_book_region_without_shortcuts() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let cfg = config(&dir, "owner", &vault);
    let client = unlocked(&cfg, &vault);
    capture(
        &client,
        &cfg,
        "us",
        "US",
        "1",
        json!([contact("us", "ada", "Ada", "(202) 555-0100")]),
    );
    capture(
        &client,
        &cfg,
        "gb",
        "GB",
        "2",
        json!([contact("gb", "bea", "Bea", "020 7946 0018")]),
    );
    assert_eq!(
        resolve(
            &client,
            &[
                "+12025550100",
                "020 7946 0018",
                "2025550100",
                "911",
                "not a phone"
            ]
        ),
        json!({
            "matches":[
                {"address":"+12025550100","contact_id":"ada","book_id":"us","display_name":"Ada"},
                {"address":"020 7946 0018","contact_id":"bea","book_id":"gb","display_name":"Bea"}
            ],
            "ambiguous":["2025550100","911","not a phone"], "more_indexing":false
        })
    );
}

#[test]
fn exact_shared_forgotten_and_changed_numbers_do_not_leak_stale_matches() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let cfg = config(&dir, "owner", &vault);
    let client = unlocked(&cfg, &vault);
    capture(
        &client,
        &cfg,
        "book",
        "US",
        "1",
        json!([
            contact("book", "one", "One", "+12025550100"),
            contact("book", "two", "Two", "+12025550100"),
            contact("book", "short", "Short", "*123")
        ]),
    );
    assert_eq!(
        resolve(&client, &["+12025550100", "*123"]),
        json!({
            "matches":[{"address":"*123","contact_id":"short","book_id":"book","display_name":"Short"}],
            "ambiguous":["+12025550100"], "more_indexing":false
        })
    );
    capture(
        &client,
        &cfg,
        "book",
        "US",
        "2",
        json!([contact("book", "one", "One", "+12025550101")]),
    );
    assert_eq!(
        resolve(&client, &["+12025550100", "+12025550101"]),
        json!({
            "matches":[
                {"address":"+12025550100","contact_id":"two","book_id":"book","display_name":"Two"},
                {"address":"+12025550101","contact_id":"one","book_id":"book","display_name":"One"}
            ],
            "ambiguous":[], "more_indexing":false
        })
    );
    client
        .forget_contact_book(&json!({"book_id":"book"}).to_string())
        .unwrap();
    assert_eq!(
        resolve(&client, &["+12025550101"]),
        json!({"matches":[],"ambiguous":["+12025550101"],"more_indexing":false})
    );
}

#[test]
fn updated_and_deleted_contacts_stop_resolving_before_index_cleanup() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let cfg = config(&dir, "owner", &vault);
    let client = unlocked(&cfg, &vault);
    let contacts = (0..40)
        .map(|number| {
            contact(
                "book",
                &format!("c{number:02}"),
                &format!("Contact {number}"),
                &format!("+1202555{number:04}"),
            )
        })
        .collect::<Vec<_>>();
    capture(
        &client,
        &cfg,
        "book",
        "US",
        "1",
        Value::Array(contacts.clone()),
    );
    assert_eq!(
        resolve(&client, &["+12025550000"]),
        json!({"matches":[{"address":"+12025550000","contact_id":"c00","book_id":"book","display_name":"Contact 0"}],"ambiguous":[],"more_indexing":false})
    );

    capture(
        &client,
        &cfg,
        "book",
        "US",
        "2",
        json!([contact("book", "c00", "Contact 0", "+12025559999")]),
    );
    assert_eq!(
        resolve(&client, &["+12025550000", "+12025559999"]),
        json!({"matches":[{"address":"+12025559999","contact_id":"c00","book_id":"book","display_name":"Contact 0"}],"ambiguous":["+12025550000"],"more_indexing":false})
    );

    client
        .begin_contact_scan(
            &json!({"schema_version":1,"scan_id":"scan","book_id":"book","generation":"2","access":"full","authoritative":true}).to_string(),
        )
        .unwrap();
    for contact in contacts.iter().skip(1) {
        client
            .observe_contact_scan(
                &json!({"schema_version":1,"scan_id":"scan","contact":contact}).to_string(),
            )
            .unwrap();
    }
    client
        .finish_contact_scan(
            &json!({"schema_version":1,"scan_id":"scan","complete":true}).to_string(),
        )
        .unwrap();
    assert_eq!(
        resolve(&client, &["+12025559999"]),
        json!({"matches":[],"ambiguous":["+12025559999"],"more_indexing":false})
    );
}
