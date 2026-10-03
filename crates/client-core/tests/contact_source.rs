//! Native source identity mappings use real SQLCipher-backed Clients.
mod common;
use common::*;
use serde_json::{Value, json};
use tempfile::TempDir;

fn call(result: Result<String, Error>) -> Value {
    serde_json::from_str(&result.unwrap()).unwrap()
}
fn book(config: &ClientConfig, id: &str) -> Value {
    json!({"id":id,"owner_device_id":config.device_id.to_string(),"generation":"1","state":"active"})
}
fn source(key: &str, name: &str, phone: &str) -> Value {
    json!({"source_key":key,"fields":{"display_name":name,"name":{"given":name},"phones":[{"id":phone,"label":"mobile","value":"+12025550100"}],"emails":[],"addresses":[]}})
}
fn capture(client: &Client, book: Value, contacts: Vec<Value>) -> Value {
    call(client.capture_platform_contacts_json(
        &json!({"schema_version":1,"book":book,"contacts":contacts}).to_string(),
    ))
}

#[test]
fn source_capture_mints_stable_ids_revisions_and_field_context_across_reopen() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let config = config(&dir, "owner", &vault);
    let first = unlocked(&config, &vault);
    let b = book(&config, "source-book");
    let captured = capture(
        &first,
        b.clone(),
        vec![source("native-1", "Ada", "phone-1")],
    );
    let id = captured["contacts"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(captured["contacts"][0]["revision"], "1");
    assert_eq!(
        capture(
            &first,
            b.clone(),
            vec![source("native-1", "Ada", "phone-1")]
        )["contacts"][0]["revision"],
        "1"
    );
    assert_eq!(
        capture(
            &first,
            b.clone(),
            vec![source("native-1", "Augusta", "phone-1")]
        )["contacts"][0]["revision"],
        "2"
    );
    let context = call(first.contact_source_context_json(
        &json!({"schema_version":1,"book_id":"source-book","contact_id":id}).to_string(),
    ));
    let field_id = context["field_sources"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(context["observed"]["phones"][0]["id"], "phone-1");
    drop(first);

    let reopened = unlocked(&config, &vault);
    let replay = capture(&reopened, b, vec![source("native-1", "Augusta", "phone-1")]);
    assert_eq!(replay["contacts"][0]["id"], id);
    assert_eq!(replay["contacts"][0]["revision"], "2");
    let context = call(reopened.contact_source_context_json(
        &json!({"schema_version":1,"book_id":"source-book","contact_id":id}).to_string(),
    ));
    assert_eq!(context["field_sources"][0]["id"], field_id);
}

#[test]
fn source_keys_are_book_scoped_and_remote_books_have_no_source_context() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let owner_config = config(&dir, "owner", &vault);
    let remote_config = config(&dir, "remote", &vault);
    let owner = unlocked(&owner_config, &vault);
    let remote = unlocked(&remote_config, &vault);
    let first = capture(
        &owner,
        book(&owner_config, "one"),
        vec![source("same-native-key", "Ada", "phone")],
    );
    let second = capture(
        &owner,
        book(&owner_config, "two"),
        vec![source("same-native-key", "Ada", "phone")],
    );
    assert_ne!(first["contacts"][0]["id"], second["contacts"][0]["id"]);
    let mut server = Server::default();
    server.upload(&owner);
    server.sync(&remote);
    let id = first["contacts"][0]["id"].as_str().unwrap();
    assert!(
        remote
            .contact_source_context_json(
                &json!({"schema_version":1,"book_id":"one","contact_id":id}).to_string()
            )
            .is_err()
    );
    assert!(
        remote
            .capture_platform_contacts_json(
                &json!({"schema_version":1,"book":book(&owner_config, "one"),"contacts":[]})
                    .to_string()
            )
            .is_err()
    );
}

fn with_provenance(mut entry: Value, provenance: Value) -> Value {
    entry["provenance"] = provenance;
    entry
}

#[test]
fn legacy_lookup_keyed_mapping_migrates_exactly_once_to_raw_identity_key() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let config = config(&dir, "owner", &vault);
    let client = unlocked(&config, &vault);
    let b = book(&config, "migrate-book");
    // Legacy mapping: keyed by an Android lookup key, no raw identity.
    let legacy = capture(&client, b.clone(), vec![source("lookup-A", "Ada", "p1")]);
    let id = legacy["contacts"][0]["id"].as_str().unwrap().to_owned();

    // A hint that is not an existing key never renames anything (no guessing).
    let other = capture(
        &client,
        b.clone(),
        vec![with_provenance(
            source("raw:20", "Bob", "p2"),
            json!({"raw_ids":["20"],"lookup_hint":"lookup-unknown"}),
        )],
    );
    assert_ne!(other["contacts"][0]["id"], id);

    // The exact legacy key as hint, with raw identity: remapped, same core contact.
    let migrated = capture(
        &client,
        b.clone(),
        vec![with_provenance(
            source("raw:10", "Ada", "p1"),
            json!({"raw_ids":["10","11"],"lookup_hint":"lookup-A"}),
        )],
    );
    assert_eq!(migrated["contacts"][0]["id"], id);
    assert_eq!(migrated["contacts"][0]["changed"], false);
    let context = call(client.contact_source_context_json(
        &json!({"schema_version":1,"book_id":"migrate-book","source_key":"raw:10"}).to_string(),
    ));
    assert_eq!(context["contact_id"], id);
    assert_eq!(context["provenance"]["raw_ids"], json!(["10", "11"]));

    // A raw-identity mapping is never renamed by a later hint pointing at it.
    let again = capture(
        &client,
        b.clone(),
        vec![with_provenance(
            source("raw:99", "Ada", "p1"),
            json!({"raw_ids":["99"],"lookup_hint":"raw:10"}),
        )],
    );
    assert_ne!(again["contacts"][0]["id"], id);

    // Malformed raw identity is rejected.
    for bad in [
        json!({"raw_ids":[]}),
        json!({"raw_ids":["01"]}),
        json!({"raw_ids":["1","1"]}),
        json!({"raw_ids":[1]}),
    ] {
        assert!(client
            .capture_platform_contacts_json(
                &json!({"schema_version":1,"book":b,"contacts":[with_provenance(source("raw:7", "X", "p7"), bad)]}).to_string()
            )
            .is_err());
    }
}
