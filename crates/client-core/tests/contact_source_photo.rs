//! Platform capture with lazily loaded photos: text-only rescans keep the stored photo and
//! owner-local photo fingerprint; explicit null removes the photo; source-key context is
//! owner-only; the fingerprint never reaches peers or views.
mod common;
use common::*;
use serde_json::{Value, json};
use tempfile::TempDir;

const BOOK: &str = "src-photo-book";

fn call(result: Result<String, Error>) -> Value {
    serde_json::from_str(&result.unwrap()).unwrap()
}
fn book(config: &ClientConfig) -> Value {
    json!({"id": BOOK, "owner_device_id": config.device_id.to_string(), "generation": "1", "state": "active"})
}
fn entry(fields_extra: Value, provenance: Option<Value>) -> Value {
    let mut fields = json!({"display_name": "Ada", "name": {"given": "Ada"}, "phones": [{"id": "p1", "label": "mobile", "value": "+12025550100"}], "emails": [], "addresses": []});
    for (k, v) in fields_extra.as_object().unwrap() {
        fields[k] = v.clone();
    }
    let mut e = json!({"source_key": "native-ada", "fields": fields});
    if let Some(p) = provenance {
        e["provenance"] = p;
    }
    e
}
fn capture(client: &Client, config: &ClientConfig, e: Value) -> Value {
    call(client.capture_platform_contacts_json(
        &json!({"schema_version": 1, "book": book(config), "contacts": [e]}).to_string(),
    ))["contacts"][0]
        .clone()
}
fn view(client: &Client) -> String {
    client
        .contact_book_view(&json!({"schema_version": 1, "book_id": BOOK}).to_string())
        .unwrap()
}
fn context(client: &Client, key: &str) -> Result<String, Error> {
    client.contact_source_context_json(
        &json!({"schema_version": 1, "book_id": BOOK, "source_key": key}).to_string(),
    )
}
fn photo_file(dir: &TempDir) -> std::path::PathBuf {
    let path = dir.path().join("ada.png");
    image::RgbImage::from_pixel(64, 64, image::Rgb([200, 10, 10]))
        .save_with_format(&path, image::ImageFormat::Png)
        .unwrap();
    path
}

#[test]
fn text_only_rescan_keeps_photo_revision_and_private_hash_explicit_null_removes() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let owner_cfg = config(&dir, "owner", &vault);
    let peer_cfg = config(&dir, "peer", &vault);
    let owner = unlocked(&owner_cfg, &vault);
    let peer = unlocked(&peer_cfg, &vault);
    let mut server = Server::default();

    // Text first.
    let first = capture(&owner, &owner_cfg, entry(json!({}), None));
    assert_eq!(first["revision"], "1");
    let contact_id = first["id"].as_str().unwrap().to_owned();
    // Photo loaded lazily, with a native fingerprint of the provider bytes.
    let photo = owner
        .prepare_contact_photo(&photo_file(&dir))
        .unwrap()
        .attachment_id;
    let with_photo = capture(
        &owner,
        &owner_cfg,
        entry(
            json!({"photo": {"attachment_id": photo.to_string()}}),
            Some(json!({"account_id": "acct-1", "photo_source_hash": "sha256:feedface"})),
        ),
    );
    assert_eq!(with_photo["revision"], "2");

    // Native compares the fingerprint by source key before deciding to re-prepare.
    let ctx: Value = serde_json::from_str(&context(&owner, "native-ada").unwrap()).unwrap();
    assert_eq!(ctx["contact_id"], contact_id);
    assert_eq!(ctx["provenance"]["photo_source_hash"], "sha256:feedface");
    assert_eq!(ctx["observed"]["photo"]["attachment_id"], photo.to_string());

    // Text-only rescans (no photo, no/partial provenance) change nothing.
    for provenance in [
        None,
        Some(json!({"account_id": "acct-1"})),
        Some(json!({"read_only": false})),
    ] {
        let rescan = capture(&owner, &owner_cfg, entry(json!({}), provenance));
        assert_eq!(rescan["revision"], "2");
        assert_eq!(rescan["changed"], false);
    }
    let ctx: Value = serde_json::from_str(&context(&owner, "native-ada").unwrap()).unwrap();
    assert_eq!(ctx["provenance"]["photo_source_hash"], "sha256:feedface");
    assert_eq!(ctx["provenance"]["account_id"], "acct-1");
    let v: Value = serde_json::from_str(&view(&owner)).unwrap();
    assert_eq!(
        v["contacts"][0]["photo"]["attachment_id"],
        photo.to_string()
    );
    let state: Value =
        serde_json::from_str(&owner.contact_photo_transfer_state_json().unwrap()).unwrap();
    assert_eq!(
        state["reclaims"],
        json!([]),
        "no reclaim from a text-only rescan"
    );

    // Publish to a peer: neither the fingerprint nor provenance reaches peer state or views.
    let remote = uuid::Uuid::new_v4().to_string();
    owner.mark_attachment_uploaded(photo, &remote).unwrap();
    let state: Value =
        serde_json::from_str(&owner.contact_photo_transfer_state_json().unwrap()).unwrap();
    for r in state["registrations"].as_array().unwrap() {
        call(owner.acknowledge_contact_photo_reference_json(
            &json!({"schema_version": 1, "envelope_id": r["envelope_id"], "attachment_id": r["attachment_id"]}).to_string(),
        ));
    }
    server.upload(&owner);
    assert_eq!(server.sync(&peer).quarantined, 0);
    let peer_view = view(&peer);
    let peer_v: Value = serde_json::from_str(&peer_view).unwrap();
    assert_eq!(
        peer_v["contacts"][0]["photo"]["attachment_id"],
        photo.to_string()
    );
    for text in [peer_view, view(&owner)] {
        assert!(
            !text.contains("feedface")
                && !text.contains("photo_source_hash")
                && !text.contains("acct-1"),
            "{text}"
        );
    }
    let wire = serde_json::to_string(&server.log).unwrap();
    assert!(!wire.contains("feedface"));

    // Explicit null removes the photo (new revision) and clears the fingerprint.
    let removed = capture(&owner, &owner_cfg, entry(json!({"photo": null}), None));
    assert_eq!(removed["revision"], "3");
    let v: Value = serde_json::from_str(&view(&owner)).unwrap();
    assert!(v["contacts"][0].get("photo").is_none());
    let ctx: Value = serde_json::from_str(&context(&owner, "native-ada").unwrap()).unwrap();
    assert!(ctx["provenance"].get("photo_source_hash").is_none());
    assert_eq!(ctx["provenance"]["account_id"], "acct-1");
    // And a later text-only rescan does not resurrect it.
    assert_eq!(
        capture(&owner, &owner_cfg, entry(json!({}), None))["revision"],
        "3"
    );
}

#[test]
fn source_key_context_is_owner_only_and_exclusive() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let owner_cfg = config(&dir, "owner", &vault);
    let peer_cfg = config(&dir, "peer", &vault);
    let owner = unlocked(&owner_cfg, &vault);
    let peer = unlocked(&peer_cfg, &vault);
    let mut server = Server::default();
    let id = capture(&owner, &owner_cfg, entry(json!({}), None))["id"].clone();
    server.upload(&owner);
    server.sync(&peer);

    // Peer (not the book owner) learns nothing, by source key or contact id.
    assert!(matches!(
        context(&peer, "native-ada"),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        peer.contact_source_context_json(
            &json!({"schema_version": 1, "book_id": BOOK, "contact_id": id}).to_string()
        ),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        context(&owner, "native-unknown"),
        Err(Error::NotFound)
    ));
    // Exactly one selector.
    assert!(matches!(
        owner.contact_source_context_json(&json!({"schema_version": 1, "book_id": BOOK, "contact_id": id, "source_key": "native-ada"}).to_string()),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        owner.contact_source_context_json(
            &json!({"schema_version": 1, "book_id": BOOK}).to_string()
        ),
        Err(Error::InvalidRequest(_))
    ));
    // Same answer by either selector.
    let by_key: Value = serde_json::from_str(&context(&owner, "native-ada").unwrap()).unwrap();
    let by_id: Value = serde_json::from_str(
        &owner
            .contact_source_context_json(
                &json!({"schema_version": 1, "book_id": BOOK, "contact_id": id}).to_string(),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(by_key, by_id);
    // A malformed fingerprint is rejected.
    let bad = owner.capture_platform_contacts_json(
        &json!({"schema_version": 1, "book": book(&owner_cfg), "contacts": [entry(json!({}), Some(json!({"photo_source_hash": ""})))]}).to_string(),
    );
    assert!(matches!(bad, Err(Error::InvalidRequest(_))));
}
