//! Contact adapter tests. Mapper output is sent through the real `Client::request_contact_edit`
//! of an unlocked desktop session and delivered to a second core client acting as the owner
//! phone, whose permit shows exactly what the phone would apply.
use super::*;
use crate::{
    media::tests::sample_png,
    tests::{fixture_at, open, Fixture, PHRASE},
};
use peppy_client_core::{ClientConfig, Cursor, DatabaseKey, DeviceId, Envelope, VaultId};
use serde_json::json;
use std::sync::{Arc, Mutex as StdMutex};

const BOOK: &str = "book-1";

fn owner_book(owner: &str, caps: Value, mode: &str) -> Value {
    json!({
        "id": BOOK, "owner_device_id": owner, "generation": "1", "state": "active",
        "capabilities": caps,
        "accounts": [{"id": "native:acct-secret", "name": "Google", "writable": true}],
        "default_account_id": "native:acct-secret",
        "policy": {"remote_edits": mode, "large_delete_requires_approval": true}
    })
}

fn ada() -> Value {
    json!({
        "id": "c1", "book_id": BOOK, "display_name": "Ada Lovelace",
        "name": {"given": "Ada", "family": "Lovelace", "middle": "King"},
        "phones": [{"id": "p1", "label": "mobile", "value": "+12025550100"}],
        "emails": [{"id": "e1", "value": "ada@example.com"}],
        "addresses": [{"id": "a1", "label": "home", "street": "1 Analytical Way", "city": "London", "iso_country_code": "gb"}],
        "notes": "Met at the Royal Society",
        "provenance": {"source_id": "native:src-c1", "account_id": "native:acct-secret", "read_only": false}
    })
}

struct Harness {
    f: Fixture,
    session: Arc<Session>,
    owner: Client,
    owner_id: String,
    log: Vec<Envelope>,
}

impl Harness {
    fn new(caps: Value, mode: &str) -> Self {
        Self::with_origin(caps, mode, "http://127.0.0.1:9")
    }

    fn with_origin(caps: Value, mode: &str, origin: &str) -> Self {
        let f = fixture_at(origin);
        let session = Arc::new(open(&f, &f.binding, &[]));
        session
            .client
            .unlock(&f.profile, &f.header, PHRASE)
            .unwrap();
        let owner_device = DeviceId::new();
        let owner = Client::open(
            ClientConfig {
                database_path: f.dir.path().join("owner-phone.db"),
                vault_id: VaultId::from_str(&f.binding.vault_id).unwrap(),
                device_id: owner_device,
            },
            DatabaseKey::new(&[7; 32]).unwrap(),
        )
        .unwrap();
        owner.unlock(&f.profile, &f.header, PHRASE).unwrap();
        owner.set_server_compaction_state(true, true).unwrap();
        session
            .client
            .set_server_compaction_state(true, true)
            .unwrap();
        let owner_id = owner_device.to_string();
        owner
            .capture_contact_book(
                &json!({"schema_version": 1, "book": owner_book(&owner_id, caps, mode), "contacts": [ada()]})
                    .to_string(),
            )
            .unwrap();
        let mut harness = Self {
            f,
            session,
            owner,
            owner_id,
            log: Vec::new(),
        };
        harness.sync_to_desktop();
        harness
    }

    /// Stands in for the media worker: records each photo reference as accepted by the server
    /// (`POST /v1/attachments/{id}/references` → 204), which releases held envelopes.
    fn acknowledge_photo_references(client: &Client) {
        let state: Value =
            serde_json::from_str(&client.contact_photo_transfer_state_json().unwrap()).unwrap();
        for registration in state["registrations"].as_array().unwrap() {
            client
                .acknowledge_contact_photo_reference_json(
                    &json!({"schema_version": 1, "envelope_id": registration["envelope_id"], "attachment_id": registration["attachment_id"]}).to_string(),
                )
                .unwrap();
        }
    }

    fn upload(log: &mut Vec<Envelope>, client: &Client) {
        Self::acknowledge_photo_references(client);
        for envelope in client.pending_outbox().unwrap() {
            if !log.iter().any(|e| e.envelope_id == envelope.envelope_id) {
                log.push(envelope.clone());
            }
            client.ack_outbox(envelope.envelope_id).unwrap();
        }
    }

    fn receive(log: &[Envelope], client: &Client) {
        let from = usize::try_from(client.receive_cursor().unwrap().0).unwrap();
        for (index, envelope) in log.iter().enumerate().skip(from) {
            client.ingest(envelope, Cursor(index as u64 + 1)).unwrap();
        }
        let report = client.apply_pending(100).unwrap();
        assert_eq!(report.quarantined, 0, "unexpected quarantine");
    }

    fn sync_to_desktop(&mut self) {
        Self::upload(&mut self.log, &self.owner);
        Self::receive(&self.log, &self.session.client);
    }

    fn sync_to_owner(&mut self) {
        Self::upload(&mut self.log, &self.session.client);
        Self::receive(&self.log, &self.owner);
    }

    fn permit(&self, request_id: &str) -> Value {
        serde_json::from_str(
            &self
                .owner
                .next_contact_apply_permit(
                    &json!({"schema_version": 1, "request_id": request_id}).to_string(),
                )
                .unwrap(),
        )
        .unwrap()
    }

    fn contact(&self) -> Value {
        list_contacts(&self.session, BOOK, None, None)
            .unwrap()
            .remove(0)
    }

    fn edit_state(&self, request_id: &str) -> String {
        list_edits(&self.session, Some(BOOK))
            .unwrap()
            .into_iter()
            .find(|edit| edit["requestId"] == request_id)
            .map(|edit| edit["state"].as_str().unwrap().to_owned())
            .unwrap()
    }
}

fn request_id(outcome: &Value) -> String {
    assert_eq!(outcome["state"], "pending");
    outcome["requestId"].as_str().unwrap().to_owned()
}

fn assert_no_private_data(text: &str, f: &Fixture) {
    for secret in [
        "native:",
        "provenance",
        "source_id",
        "account_id",
        "acct-secret",
        "descriptor",
        "file_key",
        "fileKey",
        "attachment_id",
        "attachmentId",
        "owner_device_id",
    ] {
        assert!(
            !text.contains(secret),
            "contact DTO leaked {secret}: {text}"
        );
    }
    assert!(
        !text.contains(&f.dir.path().to_string_lossy().to_string()),
        "contact DTO leaked a path"
    );
}

#[test]
fn views_are_whitelisted_and_capabilities_follow_the_owner_book() {
    let h = Harness::new(
        json!({"read": true, "write": true, "photo": true, "notes": false}),
        "auto",
    );
    let books = list_books(&h.session).unwrap();
    assert_eq!(books.len(), 1);
    assert_eq!(
        books[0]["capabilities"],
        json!({
            "canWrite": true, "canDelete": true, "canRestore": true,
            "supportsNotes": false, "supportsPhoto": true, "supportsBirthday": true
        })
    );
    assert_eq!(books[0]["contactCount"], 1);
    assert_eq!(books[0]["pendingEditCount"], 0);
    assert_eq!(books[0]["defaultAccountLabel"], "Google");
    let contact = h.contact();
    assert_eq!(contact["revision"], "1");
    assert_eq!(contact["givenName"], "Ada");
    assert_eq!(
        contact["phones"],
        json!([{"id": "p1", "label": "mobile", "displayLabel": "mobile", "number": "+12025550100", "readOnly": false}])
    );
    assert_eq!(
        contact["emails"],
        json!([{"id": "e1", "label": "", "displayLabel": "", "address": "ada@example.com", "readOnly": false}])
    );
    assert_eq!(
        contact["addresses"],
        json!([{
            "id": "a1", "label": "home", "displayLabel": "home", "street": "1 Analytical Way",
            "city": "London", "isoCountryCode": "gb", "readOnly": false
        }])
    );
    let text = serde_json::to_string(&json!([books, contact])).unwrap();
    assert_no_private_data(&text, &h.f);
}

#[test]
fn update_from_the_view_reaches_the_owner_permit_with_ids_and_expected_old() {
    let mut h = Harness::new(json!({"read": true, "write": true, "photo": true}), "auto");
    let current = h.contact();
    let mut phones = current["phones"].clone();
    phones[0]["number"] = json!("+12025550199");
    let mut addresses = current["addresses"].clone();
    addresses[0]["city"] = json!("Marylebone");
    let emails = json!([{"label": "work", "address": "ada@analytical.example"}]);
    let input = json!({
        "targetBookId": BOOK, "kind": "update", "contactId": "c1", "baseRevision": current["revision"],
        "patches": [
            {"field": "givenName", "value": "Augusta", "expectedOld": "Ada"},
            {"field": "nickname", "value": "Countess"},
            {"field": "phones", "value": phones, "expectedOld": current["phones"]},
            {"field": "emails", "value": emails, "expectedOld": current["emails"]},
            {"field": "addresses", "value": addresses, "expectedOld": current["addresses"]},
            {"field": "birthday"}
        ],
        "photo": {"kind": "keep"}
    });
    let id = request_id(&submit(&h.session, &input).unwrap());
    assert_eq!(h.edit_state(&id), "pending");
    assert_eq!(list_books(&h.session).unwrap()[0]["pendingEditCount"], 1);

    h.sync_to_owner();
    let permit = h.permit(&id);
    assert_eq!(permit["status"], "permit", "{permit}");
    let request = &permit["request"];
    assert_eq!(request["target_owner"], h.owner_id);
    assert_eq!(request["contact_id"], "c1");
    assert!(request.get("photo_op").is_none());
    let patches = request["patches"].as_array().unwrap();
    assert!(patches.contains(&json!({"op": "replace", "path": "name.given", "value": "Augusta"})));
    assert!(patches.contains(&json!({"op": "replace", "path": "nickname", "value": "Countess"})));
    assert!(patches.contains(&json!({"op": "replace", "path": "phones[p1]", "value": {"id": "p1", "label": "mobile", "value": "+12025550199"}})));
    assert!(patches.contains(&json!({"op": "remove", "path": "emails[e1]"})));
    assert!(patches.contains(&json!({"op": "replace", "path": "addresses[a1]", "value": {
        "id": "a1", "label": "home", "street": "1 Analytical Way", "city": "Marylebone", "iso_country_code": "gb"
    }})));
    let added = patches.iter().find(|p| p["op"] == "add").unwrap();
    assert_eq!(added["path"], "emails");
    assert_eq!(added["value"]["value"], "ada@analytical.example");
    assert!(uuid::Uuid::parse_str(added["value"]["id"].as_str().unwrap()).is_ok());
    assert_eq!(request["expected_old"]["name"], json!({"given": "Ada"}));
    assert_eq!(request["expected_old"]["nickname"], Value::Null);
    assert_eq!(
        request["expected_old"]["phones"],
        json!([{"id": "p1", "label": "mobile", "value": "+12025550100"}])
    );

    // The owner's applied result is the only thing that turns the request applied.
    let mut observed = ada();
    observed["name"]["given"] = json!("Augusta");
    observed["display_name"] = json!("Augusta Lovelace");
    h.owner
        .reconcile_contact_apply(&json!({"schema_version": 1, "request_id": id, "outcome": "applied", "observed": observed}).to_string())
        .unwrap();
    assert_eq!(h.edit_state(&id), "pending");
    h.sync_to_desktop();
    assert_eq!(h.edit_state(&id), "applied");
    assert_eq!(list_books(&h.session).unwrap()[0]["pendingEditCount"], 0);
    assert_eq!(h.contact()["givenName"], "Augusta");
}

#[test]
fn create_and_delete_use_the_core_request_shape_and_report_owner_states() {
    let mut h = Harness::new(json!({"read": true, "write": true, "photo": false}), "auto");
    let create = json!({
        "targetBookId": BOOK, "kind": "create",
        "patches": [
            {"field": "givenName", "value": "Grace"}, {"field": "familyName", "value": "Hopper"},
            {"field": "nickname"}, {"field": "organization", "value": ""},
            {"field": "phones", "value": [{"label": "mobile", "number": "+12025550111"}, {"label": "home", "number": ""}]},
            {"field": "emails", "value": []},
            {"field": "addresses", "value": [{"label": "work", "city": "Arlington"}]},
            {"field": "birthday", "value": {"month": "", "day": ""}},
            {"field": "notes", "value": "Admiral"}
        ],
        "photo": {"kind": "keep"}
    });
    let created = request_id(&submit(&h.session, &create).unwrap());
    let delete = json!({
        "targetBookId": BOOK, "kind": "delete", "contactId": "c1", "baseRevision": "1",
        "patches": [], "photo": {"kind": "keep"}
    });
    let deleted = request_id(&submit(&h.session, &delete).unwrap());
    h.sync_to_owner();

    let permit = h.permit(&created);
    assert_eq!(permit["status"], "permit", "{permit}");
    let request = &permit["request"];
    assert_eq!(request["kind"], "create");
    assert_eq!(request["display_name"], "Grace Hopper");
    assert_eq!(
        request["name"],
        json!({"given": "Grace", "family": "Hopper"})
    );
    assert_eq!(request["notes"], "Admiral");
    assert!(request.get("organization").is_none() && request.get("nickname").is_none());
    let phones = request["phones"].as_array().unwrap();
    assert_eq!(phones.len(), 1, "blank rows are dropped");
    assert_eq!(phones[0]["value"], "+12025550111");
    assert_eq!(request["addresses"][0]["city"], "Arlington");

    // A delete in a one-contact book exceeds the rolling 5% cap and waits for the owner.
    assert_eq!(h.permit(&deleted)["status"], "awaiting_approval");
    h.sync_to_desktop();
    assert_eq!(h.edit_state(&deleted), "awaiting-approval");
    assert_eq!(h.edit_state(&created), "pending");
}

#[test]
fn photo_set_is_prepared_by_core_and_held_until_its_upload_is_registered() {
    let mut h = Harness::new(json!({"read": true, "write": true, "photo": true}), "auto");
    let png = format!(
        "data:image/png;base64,{}",
        STANDARD.encode(sample_png(600, 400))
    );
    let input = json!({
        "targetBookId": BOOK, "kind": "update", "contactId": "c1", "baseRevision": "1",
        "patches": [], "photo": {"kind": "set", "croppedDataUrl": png}
    });
    let id = request_id(&submit(&h.session, &input).unwrap());
    let staging = h.session.data_dir.join(PHOTO_STAGING_DIR);
    assert_eq!(
        fs::read_dir(&staging).unwrap().count(),
        0,
        "plaintext staging must be removed"
    );

    // Held: nothing is published before the photo is uploaded and its reference registered.
    assert!(h.session.client.pending_outbox().unwrap().is_empty());
    let state: Value = serde_json::from_str(
        &h.session
            .client
            .contact_photo_transfer_state_json()
            .unwrap(),
    )
    .unwrap();
    let local =
        AttachmentId::from_str(state["uploads"][0]["attachment_id"].as_str().unwrap()).unwrap();
    let info = h.session.client.attachment_info(local).unwrap();
    assert_eq!(info.media_type, "image/jpeg");
    assert!(info.plaintext_bytes <= 64 * 1024);
    let remote = uuid::Uuid::new_v4().to_string();
    h.session
        .client
        .mark_attachment_uploaded(local, &remote)
        .unwrap();
    assert!(
        h.session.client.pending_outbox().unwrap().is_empty(),
        "still held until the reference is registered"
    );
    h.sync_to_owner();
    // The owner never authorizes a write before it holds the verified photo.
    let waiting = h.permit(&id);
    assert_eq!(waiting["status"], "waiting_media", "{waiting}");
    assert_eq!(waiting["attachment_id"], local.to_string());
    let ciphertext = h.session.client.native_cipher_file(local).unwrap();
    h.owner
        .install_downloaded_attachment(local, &ciphertext)
        .unwrap();
    let permit = h.permit(&id);
    assert_eq!(permit["status"], "permit", "{permit}");
    assert_eq!(
        permit["request"]["photo_op"],
        json!({"op": "set", "attachment_id": local.to_string()})
    );
    assert_eq!(permit["request"]["patches"], json!([]));

    // Once the phone applies it, the desktop view shows a re-encoded avatar and never the ID.
    let mut observed = ada();
    observed["photo"] = json!({"attachment_id": local.to_string()});
    h.owner
        .reconcile_contact_apply(
            &json!({"schema_version": 1, "request_id": id, "outcome": "applied", "observed": observed})
                .to_string(),
        )
        .unwrap();
    h.sync_to_desktop();
    assert_eq!(h.edit_state(&id), "applied");
    let contact = h.contact();
    assert!(contact["photoDataUrl"]
        .as_str()
        .unwrap()
        .starts_with("data:image/jpeg;base64,"));
    let encoded =
        contact["photoDataUrl"].as_str().unwrap()["data:image/jpeg;base64,".len()..].to_owned();
    assert!(STANDARD.decode(encoded).unwrap().len() <= AVATAR_MAX_BYTES);
    assert_no_private_data(&contact.to_string(), &h.f);
    assert!(!contact.to_string().contains(&local.to_string()));

    // Remove is a distinct operation and keep sends none.
    let mut remove = json!({
        "targetBookId": BOOK, "kind": "update", "contactId": "c1", "baseRevision": "1",
        "patches": [], "photo": {"kind": "remove"}
    });
    let stale = request_id(&submit(&h.session, &remove).unwrap());
    h.sync_to_owner();
    assert_eq!(h.permit(&stale)["status"], "conflict");
    remove["baseRevision"] = contact["revision"].clone();
    let removed = request_id(&submit(&h.session, &remove).unwrap());
    h.sync_to_owner();
    assert_eq!(
        h.permit(&removed)["request"]["photo_op"],
        json!({"op": "remove"})
    );
    let keep = json!({"targetBookId": BOOK, "kind": "update", "contactId": "c1", "baseRevision": "1", "patches": [], "photo": {"kind": "keep"}});
    assert_eq!(
        submit(&h.session, &keep).unwrap_err().code,
        "invalid-contact-edit"
    );
}

#[test]
fn unsupported_inputs_fail_before_anything_is_queued() {
    let h = Harness::new(
        json!({"read": true, "write": true, "photo": false, "notes": false, "birthday": false}),
        "auto",
    );
    let current = h.contact();
    let update = |patches: Value, photo: Value| json!({"targetBookId": BOOK, "kind": "update", "contactId": "c1", "baseRevision": "1", "patches": patches, "photo": photo});
    let keep = json!({"kind": "keep"});
    let code = |input: Value| submit(&h.session, &input).unwrap_err().code;
    assert_eq!(
        code(update(
            json!([{"field": "birthday", "value": {"month": 12, "day": 10}}]),
            keep.clone()
        )),
        "contact-field-unsupported"
    );
    assert_eq!(
        code(update(
            json!([{"field": "notes", "value": "x", "expectedOld": current["notes"]}]),
            keep.clone()
        )),
        "contact-field-unsupported"
    );
    let png = format!(
        "data:image/png;base64,{}",
        STANDARD.encode(sample_png(4, 4))
    );
    assert_eq!(
        code(update(
            json!([]),
            json!({"kind": "set", "croppedDataUrl": png})
        )),
        "contact-field-unsupported"
    );
    assert_eq!(
        code(update(
            json!([{"field": "phones", "value": []}]),
            keep.clone()
        )),
        "invalid-contact-edit"
    );
    assert_eq!(
        code(update(
            json!([{"field": "phones", "value": [{"id": "forged", "number": "1"}], "expectedOld": current["phones"]}]),
            keep.clone()
        )),
        "invalid-contact-edit"
    );
    assert_eq!(
        code(update(
            json!([{"field": "displayName", "value": "X"}]),
            keep.clone()
        )),
        "invalid-contact-edit"
    );
    assert_eq!(
        code(update(
            json!([{"field": "givenName", "value": "A"}, {"field": "givenName", "value": "B"}]),
            keep.clone()
        )),
        "invalid-contact-edit"
    );
    let mut bad_revision = update(json!([{"field": "givenName", "value": "A"}]), keep.clone());
    bad_revision["baseRevision"] = json!("01");
    assert_eq!(code(bad_revision), "invalid-contact-edit");
    assert!(h.session.client.pending_outbox().unwrap().is_empty());
    assert!(list_edits(&h.session, None).unwrap().is_empty());
}

#[test]
fn read_only_and_owned_books_refuse_writes_and_restore() {
    let h = Harness::new(json!({"read": true, "write": true}), "off");
    let book = &list_books(&h.session).unwrap()[0];
    assert_eq!(book["capabilities"]["canWrite"], false);
    let input = json!({"targetBookId": BOOK, "kind": "delete", "contactId": "c1", "baseRevision": "1", "photo": {"kind": "keep"}});
    assert_eq!(
        submit(&h.session, &input).unwrap_err().code,
        "contact-book-read-only"
    );
    assert_eq!(
        restore(&h.session, BOOK, "c1").unwrap_err().code,
        "contact-book-read-only"
    );
    assert!(restorable(&h.session, BOOK).unwrap().is_empty());

    let own = "11111111-1111-1111-1111-111111111111";
    let owned = owner_book(own, json!({"write": true, "photo": true}), "auto");
    let caps = capabilities(&owned, own);
    assert!(
        !caps.can_write && !caps.can_restore,
        "the owner applies locally, never by request"
    );
    let mut read_only_accounts = owner_book("phone", json!({"write": true}), "auto");
    read_only_accounts["accounts"] = json!([{"name": "Exchange", "writable": false}]);
    assert!(!capabilities(&read_only_accounts, own).can_write);
    let ios = owner_book(
        "phone",
        json!({"write": true, "photo": false, "notes": false}),
        "confirm",
    );
    assert_eq!(
        capabilities(&ios, own),
        Capabilities {
            can_write: true,
            supports_notes: false,
            supports_photo: false,
            supports_birthday: true,
            can_restore: true
        }
    );
    let mut retired = ios.clone();
    retired["state"] = json!("retired");
    assert!(!capabilities(&retired, own).can_write);
}

#[test]
fn view_marks_unrepresentable_items_read_only_and_never_exposes_photo_references() {
    let contact = json!({
        "id": "c9", "revision": "3", "display_name": "",
        "phones": [{"id": "p9", "value": "+1", "read_only": true}, {"id": "p10", "value": "+2", "primary": true}],
        "addresses": [{"id": "a9", "street": "x", "formatted": "x, y"}],
        "photo": {"attachment_id": "0190a5b2-7c1f-7e4a-9b1e-123456789abc", "available": true},
        "birthday": {"month": 1}, "source_id": "native:x", "digest": "abc"
    });
    let mut asked = Vec::new();
    let view = contact_view(&contact, BOOK, &mut |id| {
        asked.push(id);
        Some("data:image/png;base64,AA==".into())
    })
    .unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(view["displayName"], "Contact");
    assert_eq!(view["phones"][0]["readOnly"], true);
    assert_eq!(view["phones"][1]["readOnly"], true);
    assert_eq!(view["addresses"][0]["readOnly"], true);
    assert_eq!(view["photoDataUrl"], "data:image/png;base64,AA==");
    let text = view.to_string();
    for leaked in [
        "0190a5b2",
        "birthday",
        "native:",
        "digest",
        "formatted",
        "primary",
    ] {
        assert!(!text.contains(leaked), "{leaked} leaked: {text}");
    }
    let mut pending = contact.clone();
    pending["photo"]["available"] = json!(false);
    let view = contact_view(&pending, BOOK, &mut |_| {
        panic!("not decrypted while downloading")
    })
    .unwrap();
    assert_eq!(view["photoPending"], true);
    assert!(view.get("photoDataUrl").is_none());

    // Editing a read-only item is refused; leaving it untouched is fine.
    let old = json!([{"id": "p9", "label": "", "number": "+1", "readOnly": true}]);
    assert!(list_patches(
        List::Phones,
        Some(&json!([{"id": "p9", "number": "+3"}])),
        Some(&old)
    )
    .is_err());
    assert!(list_patches(List::Phones, Some(&json!([])), Some(&old)).is_err());
    let (patches, _) = list_patches(List::Phones, Some(&old), Some(&old)).unwrap();
    assert!(patches.is_empty());
}

#[test]
fn ledger_states_photo_data_urls_and_dates_map_conservatively() {
    assert_eq!(edit_state("requested", 100, 50), Some("pending"));
    assert_eq!(edit_state("requested", 100, 100), Some("expired"));
    assert_eq!(
        edit_state("awaiting_approval", 100, 50),
        Some("awaiting-approval")
    );
    assert_eq!(
        edit_state("outcome_unknown", 100, 500),
        Some("outcome-unknown")
    );
    assert_eq!(edit_state("applied", 100, 500), Some("applied"));
    assert_eq!(edit_state("mystery", 100, 50), None);
    assert_eq!(utc_date(0), "1970-01-01");
    assert_eq!(utc_date(1_709_164_800), "2024-02-29");
    assert!(decode_photo_data_url("data:image/svg+xml;base64,PHN2Zz4=").is_err());
    let gif = "data:image/png;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==";
    assert!(
        decode_photo_data_url(gif).is_err(),
        "declared type must match sniffed bytes"
    );
    let png = format!(
        "data:image/png;base64,{}",
        STANDARD.encode(sample_png(2, 2))
    );
    assert!(decode_photo_data_url(&png).is_ok());
    assert_eq!(photo_input(&json!({})).unwrap(), PhotoInput::Keep);
    assert_eq!(
        photo_input(&json!({"photo": {"kind": "remove"}})).unwrap(),
        PhotoInput::Remove
    );
}

#[test]
fn birthday_badges_and_remote_restore_round_trip_through_the_owner() {
    let mut h = Harness::new(
        json!({"read": true, "write": true, "photo": true, "birthday": true}),
        "auto",
    );
    let input = json!({
        "targetBookId": BOOK, "kind": "update", "contactId": "c1", "baseRevision": "1",
        "patches": [{"field": "birthday", "value": {"month": "12", "day": "10", "year": ""}}],
        "photo": {"kind": "keep"}
    });
    let id = request_id(&submit(&h.session, &input).unwrap());
    // The ledger names the contact, so its row carries an honest pending badge.
    let edit = list_edits(&h.session, Some(BOOK))
        .unwrap()
        .into_iter()
        .find(|e| e["requestId"] == id)
        .unwrap();
    assert_eq!(edit["contactId"], "c1");
    assert_eq!(edit["kind"], "update");
    assert_eq!(edit["summary"], "Birthday");
    let contact = h.contact();
    assert_eq!(contact["pendingEditId"], id);
    assert_eq!(contact["pendingEditState"], "pending");
    assert_eq!(contact["pendingEditSummary"], "Birthday");

    h.sync_to_owner();
    let permit = h.permit(&id);
    assert_eq!(permit["status"], "permit", "{permit}");
    assert_eq!(
        permit["request"]["patches"],
        json!([{"op": "replace", "path": "birthday", "value": {"month": 12, "day": 10}}])
    );
    assert_eq!(permit["request"]["expected_old"]["birthday"], Value::Null);
    let mut observed = ada();
    observed["birthday"] = json!({"month": 12, "day": 10});
    h.owner
        .reconcile_contact_apply(
            &json!({"schema_version": 1, "request_id": id, "outcome": "applied", "observed": observed})
                .to_string(),
        )
        .unwrap();
    h.sync_to_desktop();
    let contact = h.contact();
    assert_eq!(contact["birthday"], json!({"month": 12, "day": 10}));
    assert!(
        contact.get("pendingEditState").is_none(),
        "applied clears the badge"
    );

    // Delete through the owner, then ask it to restore from this computer.
    let delete = json!({
        "targetBookId": BOOK, "kind": "delete", "contactId": "c1",
        "baseRevision": contact["revision"], "photo": {"kind": "keep"}
    });
    let deleted = request_id(&submit(&h.session, &delete).unwrap());
    h.sync_to_owner();
    assert_eq!(h.permit(&deleted)["status"], "awaiting_approval");
    h.owner
        .contact_approval_json(&json!({"request_id": deleted, "approve": true}).to_string())
        .unwrap();
    assert_eq!(h.permit(&deleted)["status"], "permit");
    h.owner
        .reconcile_contact_apply(
            &json!({"schema_version": 1, "request_id": deleted, "outcome": "applied"}).to_string(),
        )
        .unwrap();
    h.sync_to_desktop();
    assert!(list_contacts(&h.session, BOOK, None, None)
        .unwrap()
        .is_empty());
    let restorable = restorable(&h.session, BOOK).unwrap();
    assert_eq!(restorable.len(), 1);
    assert_eq!(restorable[0]["displayName"], "Ada Lovelace");
    let restored = request_id(&restore(&h.session, BOOK, "c1").unwrap());
    assert_eq!(h.edit_state(&restored), "pending");
    h.sync_to_owner();
    let permit = h.permit(&restored);
    assert_eq!(permit["status"], "permit", "{permit}");
    assert_eq!(permit["request"]["kind"], "create");
    assert_eq!(
        permit["request"]["birthday"],
        json!({"month": 12, "day": 10})
    );
    assert_no_private_data(&serde_json::to_string(&restorable).unwrap(), &h.f);
}

#[test]
fn raw_platform_labels_get_display_labels_and_birthdays_are_validated() {
    assert_eq!(display_label("_$!<Mobile>!$_"), "mobile");
    assert_eq!(display_label("work"), "work");
    assert_eq!(display_label("Custom label"), "Custom label");
    let item = item_view(
        List::Phones,
        &json!({"id": "p", "label": "_$!<Main>!$_", "value": "1"}),
    )
    .unwrap();
    assert_eq!(
        item["label"], "_$!<Main>!$_",
        "the wire label round-trips unchanged"
    );
    assert_eq!(item["displayLabel"], "main");
    assert_eq!(
        ui_birthday(Some(&json!({"month": "", "day": null}))).unwrap(),
        None
    );
    assert_eq!(
        ui_birthday(Some(&json!({"month": 2, "day": "29"}))).unwrap(),
        Some(json!({"month": 2, "day": 29}))
    );
    assert!(ui_birthday(Some(&json!({"month": 13, "day": 1}))).is_err());
    assert!(ui_birthday(Some(&json!({"day": 1}))).is_err());
    assert!(ui_birthday(Some(&json!({"month": 1, "day": 1, "era": 1}))).is_err());
}

#[test]
fn names_resolve_for_display_and_recipient_search_returns_phone_addresses() {
    let h = Harness::new(json!({"read": true, "write": true}), "auto");
    let resolved = resolve_addresses(
        &h.session.client,
        &[
            ("+12025550100".into(), Some(h.owner_id.clone())),
            ("+19995550000".into(), None),
            ("not a number".into(), None),
        ],
    );
    assert_eq!(
        resolved.len(),
        1,
        "unknown numbers and non-numbers stay raw"
    );
    assert_eq!(resolved["+12025550100"].display_name, "Ada Lovelace");
    assert_eq!(resolved["+12025550100"].contact_id, "c1");

    let snapshot = snapshot_contacts(&h.session, &[("+12025550100".into(), None)]);
    let resolution = snapshot.resolution.unwrap();
    assert_eq!(resolution["+12025550100"]["displayName"], "Ada Lovelace");
    assert_eq!(snapshot.books.unwrap().len(), 1);
    assert_eq!(snapshot.pending_count, Some(0));
    assert_eq!(snapshot.sync.unwrap()["repairRequired"], false);

    let found = search_recipients(&h.session, "lovelace", None).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0]["address"], "+12025550100",
        "the recipient is the phone address"
    );
    assert_eq!(found[0]["displayName"], "Ada Lovelace");
    assert_eq!(found[0]["label"], "mobile");
    // JSON keys never match: every stored contact has a "phones" key.
    assert!(search_recipients(&h.session, "phones", None)
        .unwrap()
        .is_empty());
    assert!(search_recipients(&h.session, "  ", None)
        .unwrap()
        .is_empty());
    let text = serde_json::to_string(&json!([found, resolution])).unwrap();
    assert_no_private_data(&text, &h.f);

    assert_eq!(request_repair(&h.session).unwrap()["repairRequired"], true);
    assert!(h.session.client.contact_repair_required().unwrap());
}

/// One recorded request to the mock server.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub method: String,
    pub path: String,
    pub body: String,
}

/// Minimal HTTP/1.1 server (one request per connection) answering through `route`.
pub(crate) async fn mock_server(
    route: impl Fn(&str, &str, &str) -> (u16, String) + Send + Sync + 'static,
) -> (String, Arc<StdMutex<Vec<Seen>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let (route, record) = (Arc::new(route), seen.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (route, record) = (route.clone(), record.clone());
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buffer = [0u8; 8192];
                let header_end = loop {
                    let n = socket.read(&mut buffer).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buffer[..n]);
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let head = String::from_utf8_lossy(&data[..header_end]).to_string();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                while data.len() < header_end + length {
                    let n = socket.read(&mut buffer).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buffer[..n]);
                }
                let mut parts = head.split_whitespace();
                let (method, path) = (
                    parts.next().unwrap_or("").to_owned(),
                    parts.next().unwrap_or("").to_owned(),
                );
                let body = String::from_utf8_lossy(&data[header_end..]).to_string();
                let (status, reply) = route(&method, &path, &body);
                record.lock().unwrap().push(Seen { method, path, body });
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("http://{address}"), seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_round_reserves_tracked_photos_and_registers_references_before_publishing() {
    let (origin, seen) = mock_server(|method, path, body| {
        let id = path.split('/').nth(3).unwrap_or("").to_owned();
        match (method, path) {
            ("POST", "/v1/attachments/reserve") => {
                let request: Value = serde_json::from_str(body).unwrap();
                (200, json!({"attachment_id": request["attachment_id"], "expires_in_seconds": 600}).to_string())
            }
            ("PUT", p) if p.ends_with("/upload") => (200, "{}".into()),
            ("POST", p) if p.ends_with("/finalize") => {
                (200, json!({"attachment_id": id, "duplicate": false}).to_string())
            }
            ("POST", p) if p.ends_with("/references") => (204, String::new()),
            ("POST", "/v1/events") | ("POST", "/v1/commands") => (200, "{}".into()),
            ("POST", "/v1/compaction/capability") => (204, String::new()),
            ("GET", "/v1/snapshot") => (200, json!({"high_water_cursor": "0", "record_count": "0", "compaction_supported": true, "compaction_active": true, "compaction_generation": "0"}).to_string()),
            _ => (404, json!({"code": "not_found"}).to_string()),
        }
    })
    .await;
    let h = tokio::task::spawn_blocking(move || {
        Harness::with_origin(
            json!({"read": true, "write": true, "photo": true}),
            "auto",
            &origin,
        )
    })
    .await
    .unwrap();
    let png = format!(
        "data:image/png;base64,{}",
        STANDARD.encode(sample_png(300, 300))
    );
    let input = json!({
        "targetBookId": BOOK, "kind": "update", "contactId": "c1", "baseRevision": "1",
        "patches": [], "photo": {"kind": "set", "croppedDataUrl": png}
    });
    let session = h.session.clone();
    let outcome = tokio::task::spawn_blocking(move || submit(&session, &input))
        .await
        .unwrap()
        .unwrap();
    request_id(&outcome);

    crate::sync::work_round(&h.session).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    let reserve = seen
        .iter()
        .find(|s| s.path == "/v1/attachments/reserve")
        .expect("photo reserved");
    let reserve: Value = serde_json::from_str(&reserve.body).unwrap();
    assert_eq!(
        reserve["reference_tracking"], true,
        "contact photos are reference-tracked"
    );
    let references = seen
        .iter()
        .position(|s| s.path.ends_with("/references"))
        .expect("reference registered");
    let published = seen
        .iter()
        .position(|s| s.path == "/v1/events")
        .expect("request published");
    assert!(references < published, "registration precedes publication");
    let body: Value = serde_json::from_str(&seen[references].body).unwrap();
    assert_eq!(
        body["references"][0]["producer_device_id"],
        h.f.binding.device_id
    );
    assert!(
        seen.iter().all(|s| !s.path.contains("public-copies")),
        "no public copy"
    );
    let state: Value = serde_json::from_str(
        &h.session
            .client
            .contact_photo_transfer_state_json()
            .unwrap(),
    )
    .unwrap();
    assert!(state["registrations"].as_array().unwrap().is_empty());
    assert!(h.session.client.pending_outbox().unwrap().is_empty());
}
