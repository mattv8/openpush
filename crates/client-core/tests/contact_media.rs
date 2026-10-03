//! Contact photos end to end through the real sealed outbox: normalized photo -> encrypted
//! attachment -> held contact event -> upload -> durable reference registration -> publish ->
//! peer descriptor ingest -> download/install -> decrypt. Also GC references and reclaims.

mod common;
use common::*;
use serde_json::{Value, json};
use std::path::Path;
use tempfile::TempDir;

const BOOK: &str = "book1";

fn call(result: Result<String, Error>) -> Value {
    serde_json::from_str(&result.unwrap()).unwrap()
}
fn book(owner: &ClientConfig) -> Value {
    json!({
        "id": BOOK, "owner_device_id": owner.device_id.to_string(), "generation": "1",
        "state": "active", "capabilities": {"read": true, "write": true, "photo": true},
        "policy": {"remote_edits": "auto"}
    })
}
fn contact(photo: Option<AttachmentId>) -> Value {
    let mut c = json!({
        "id": "c1", "book_id": BOOK, "display_name": "Ada Lovelace",
        "name": {"given": "Ada", "family": "Lovelace"}, "phones": [], "emails": [], "addresses": [],
        "provenance": {"source_id": "native:src-c1", "account_id": "native:acct", "read_only": false}
    });
    if let Some(id) = photo {
        c["photo"] = json!({"attachment_id": id.to_string(), "state": "pending_upload"});
    }
    c
}
fn capture(
    client: &Client,
    owner: &ClientConfig,
    photo: Option<AttachmentId>,
) -> Result<String, Error> {
    client.capture_contact_book(
        &json!({"schema_version": 1, "book": book(owner), "contacts": [contact(photo)]})
            .to_string(),
    )
}
fn view_raw(client: &Client) -> String {
    client
        .contact_book_view(&json!({"schema_version": 1, "book_id": BOOK}).to_string())
        .unwrap()
}
fn find(client: &Client, id: &str) -> Option<Value> {
    serde_json::from_str::<Value>(&view_raw(client)).unwrap()["contacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .cloned()
}
fn state(client: &Client) -> Value {
    call(client.contact_photo_transfer_state_json())
}
fn photo_file(dir: &Path, name: &str, seed: u8) -> std::path::PathBuf {
    let image = image::RgbImage::from_fn(300, 200, |x, y| {
        image::Rgb([(x as u8).wrapping_add(seed), y as u8, seed])
    });
    let path = dir.join(name);
    image
        .save_with_format(&path, image::ImageFormat::Png)
        .unwrap();
    path
}
fn no_media_secret(text: &str) {
    for needle in [
        "descriptor",
        "file_key",
        "remote_object_id",
        "ciphertext_sha256",
    ] {
        assert!(!text.contains(needle), "{needle} leaked in {text}");
    }
}
/// UI views additionally never carry provider identity.
fn no_secret(text: &str) {
    no_media_secret(text);
    assert!(!text.contains("native:"), "provenance leaked in {text}");
}
fn ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn outbox_ids(client: &Client) -> std::collections::BTreeSet<String> {
    client
        .pending_outbox()
        .unwrap()
        .iter()
        .map(|e| e.envelope_id.to_string())
        .collect()
}
fn remote_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

struct Pair {
    dir: TempDir,
    vault: Vault,
    owner_cfg: ClientConfig,
    owner: Client,
    peer: Client,
    server: Server,
}
fn pair() -> Pair {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let owner_cfg = config(&dir, "owner", &vault);
    let peer_cfg = config(&dir, "peer", &vault);
    let owner = unlocked(&owner_cfg, &vault);
    let peer = unlocked(&peer_cfg, &vault);
    Pair {
        dir,
        vault,
        owner_cfg,
        owner,
        peer,
        server: Server::default(),
    }
}
impl Pair {
    fn deliver(&mut self, owner_to_peer: bool) {
        let (from, to) = if owner_to_peer {
            (&self.owner, &self.peer)
        } else {
            (&self.peer, &self.owner)
        };
        self.server.upload(from);
        let report = self.server.sync(to);
        assert_eq!(report.quarantined, 0, "unexpected quarantine");
        // Each side also applies its own echoed events.
        self.server.sync(from);
    }
    /// Native upload: mark uploaded, then POST + acknowledge every pending registration.
    fn upload_and_register(&self, client: &Client, id: AttachmentId) -> String {
        let remote = remote_id();
        client.mark_attachment_uploaded(id, &remote).unwrap();
        self.register_all(client);
        remote
    }
    fn register_all(&self, client: &Client) {
        for registration in state(client)["registrations"].as_array().unwrap() {
            call(client.acknowledge_contact_photo_reference_json(
                &json!({"schema_version": 1, "envelope_id": registration["envelope_id"], "attachment_id": registration["attachment_id"]}).to_string(),
            ));
        }
    }
}

fn plaintext(client: &Client, id: AttachmentId) -> Vec<u8> {
    let file = client.open_native_plaintext(id).unwrap();
    std::fs::read(file.path()).unwrap()
}

#[test]
fn photo_roundtrip_holds_until_upload_and_registration_then_peer_decrypts() {
    let mut p = pair();
    let source = photo_file(p.dir.path(), "ada.png", 7);
    let id = p
        .owner
        .prepare_contact_photo(&source)
        .unwrap()
        .attachment_id;
    call(capture(&p.owner, &p.owner_cfg, Some(id)));

    // The photo-bearing upsert is held; only the book state is published.
    assert_eq!(p.server.upload(&p.owner), 1);
    p.server.sync(&p.peer);
    assert!(find(&p.peer, "c1").is_none());
    let s = state(&p.owner);
    assert_eq!(
        s["uploads"],
        json!([{"attachment_id": id.to_string(), "reference_tracking": true}])
    );
    assert_eq!(s["registrations"], json!([]));
    assert!(
        p.owner
            .pending_uploads()
            .unwrap()
            .iter()
            .any(|o| o.attachment_id == id)
    );
    assert!(!p.owner.discard_unreferenced_attachment(id).unwrap());

    // Upload seals the row, but it stays withheld until the reference is registered.
    let remote = remote_id();
    p.owner.mark_attachment_uploaded(id, &remote).unwrap();
    assert!(p.owner.pending_outbox().unwrap().is_empty());
    let s = state(&p.owner);
    assert_eq!(s["uploads"], json!([]));
    let registrations = s["registrations"].as_array().unwrap().clone();
    assert_eq!(registrations.len(), 1);
    let reg = &registrations[0];
    assert_eq!(reg["attachment_id"], remote);
    assert_eq!(reg["local_attachment_id"], id.to_string());
    assert_eq!(reg["producer_device_id"], p.owner_cfg.device_id.to_string());

    // Registration work survives a restart.
    drop(std::mem::replace(&mut p.owner, p.peer.clone()));
    p.owner = unlocked(&p.owner_cfg, &p.vault);
    assert_eq!(state(&p.owner)["registrations"], json!(registrations));
    assert!(p.owner.pending_outbox().unwrap().is_empty());

    let wrong = json!({"schema_version": 1, "envelope_id": reg["envelope_id"], "attachment_id": remote_id()});
    assert!(matches!(
        p.owner
            .acknowledge_contact_photo_reference_json(&wrong.to_string()),
        Err(Error::NotFound)
    ));
    let ack =
        json!({"schema_version": 1, "envelope_id": reg["envelope_id"], "attachment_id": remote});
    assert_eq!(
        call(
            p.owner
                .acknowledge_contact_photo_reference_json(&ack.to_string())
        )["envelope_released"],
        true
    );
    // Idempotent.
    call(
        p.owner
            .acknowledge_contact_photo_reference_json(&ack.to_string()),
    );
    let released = p.owner.pending_outbox().unwrap();
    assert_eq!(released.len(), 1);
    assert_eq!(
        released[0].envelope_id.to_string(),
        reg["envelope_id"].as_str().unwrap()
    );
    assert_eq!(
        released[0].producer_sequence.0.to_string(),
        reg["producer_sequence"].as_str().unwrap()
    );
    let wire = serde_json::to_string(&released[0]).unwrap();
    assert!(!wire.contains(&id.to_string()) && !wire.contains(&remote));

    p.deliver(true);
    let peer_contact = find(&p.peer, "c1").unwrap();
    assert_eq!(
        peer_contact["photo"],
        json!({"attachment_id": id.to_string(), "available": false})
    );
    no_secret(&view_raw(&p.peer));
    no_secret(&view_raw(&p.owner));

    // Generic download path: the peer learned the remote object and verified metadata.
    let download = p.peer.pending_downloads().unwrap();
    assert_eq!(download.len(), 1);
    assert_eq!(download[0].attachment_id, id);
    assert_eq!(
        download[0].remote_object_id.as_deref(),
        Some(remote.as_str())
    );
    let fetched = p.dir.path().join("fetched.opss");
    std::fs::copy(p.owner.native_cipher_file(id).unwrap(), &fetched).unwrap();
    p.peer.install_downloaded_attachment(id, &fetched).unwrap();
    let received = plaintext(&p.peer, id);
    assert_eq!(received, plaintext(&p.owner, id));
    let decoded = image::load_from_memory_with_format(&received, image::ImageFormat::Jpeg).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (256, 256));
    assert_eq!(find(&p.peer, "c1").unwrap()["photo"]["available"], true);

    // Fourth reference class blocks local GC on both sides; received photos are never reclaimed.
    assert!(!p.owner.discard_unreferenced_attachment(id).unwrap());
    assert!(!p.peer.discard_unreferenced_attachment(id).unwrap());
    assert_eq!(state(&p.peer)["reclaims"], json!([]));
    assert_eq!(state(&p.owner)["reclaims"], json!([]));
}

#[test]
fn replaced_photo_is_queued_for_server_reclaim_and_tombstone_retains_for_restore() {
    let mut p = pair();
    let first = p
        .owner
        .prepare_contact_photo(&photo_file(p.dir.path(), "a.png", 1))
        .unwrap()
        .attachment_id;
    call(capture(&p.owner, &p.owner_cfg, Some(first)));
    p.upload_and_register(&p.owner, first);
    p.deliver(true);

    let second = p
        .owner
        .prepare_contact_photo(&photo_file(p.dir.path(), "b.png", 99))
        .unwrap()
        .attachment_id;
    call(capture(&p.owner, &p.owner_cfg, Some(second)));
    let reclaims = state(&p.owner)["reclaims"].as_array().unwrap().clone();
    assert_eq!(reclaims.len(), 1);
    assert_eq!(reclaims[0]["attachment_id"], first.to_string());
    assert_eq!(reclaims[0]["references"].as_array().unwrap().len(), 1);
    // Release hint: the server cursor of the (echoed) registered record; native sends
    // DELETE {compaction_generation, release_before_cursor} only once the server's
    // replay floor covers it.
    let cursor = reclaims[0]["references"][0]["source_cursor"]
        .as_str()
        .unwrap();
    assert!(cursor.parse::<u64>().unwrap() >= 1);
    assert_eq!(reclaims[0]["release_after_cursor"], cursor);
    assert_eq!(
        reclaims[0]["references"][0]["producer_device_id"],
        p.owner_cfg.device_id.to_string()
    );
    // Not deleted locally while the server has not proven release.
    assert!(!p.owner.discard_unreferenced_attachment(first).unwrap());
    let ack = |status: u64| {
        call(p.owner.acknowledge_contact_photo_reclaim_json(
            &json!({"schema_version": 1, "attachment_id": first.to_string(), "http_status": status}).to_string(),
        ))
    };
    // 409: retained and rescheduled (about an hour out), not abandoned.
    let conflict = ack(409);
    assert_eq!(conflict["status"], "pending");
    let wait = conflict["next_due_at"].as_i64().unwrap() - ts();
    assert!((3500..=3600).contains(&wait), "{wait}");
    let deferred = state(&p.owner);
    assert_eq!(deferred["reclaims"], json!([]));
    assert_eq!(deferred["reclaims_deferred"], 1);
    assert!(!p.owner.discard_unreferenced_attachment(first).unwrap());
    assert_eq!(ack(204)["status"], "completed");
    assert_eq!(ack(404)["status"], "completed");
    assert_eq!(state(&p.owner)["reclaims"], json!([]));
    assert!(p.owner.discard_unreferenced_attachment(first).unwrap());

    p.upload_and_register(&p.owner, second);
    p.deliver(true);
    assert_eq!(
        find(&p.peer, "c1").unwrap()["photo"]["attachment_id"],
        second.to_string()
    );

    // An authoritative scan without c1 tombstones it; the photo stays retained for restore.
    call(p.owner.begin_contact_scan(&json!({"schema_version": 1, "scan_id": "s1", "book_id": BOOK, "generation": "1", "access": "full", "authoritative": true}).to_string()));
    call(p.owner.finish_contact_scan(
        &json!({"schema_version": 1, "scan_id": "s1", "complete": true}).to_string(),
    ));
    // The tombstone carries an already-uploaded photo: sealed immediately, withheld for registration.
    let registrations = state(&p.owner)["registrations"].as_array().unwrap().clone();
    assert_eq!(registrations.len(), 1);
    assert!(!outbox_ids(&p.owner).contains(registrations[0]["envelope_id"].as_str().unwrap()));
    p.register_all(&p.owner);
    p.deliver(true);
    for client in [&p.owner, &p.peer] {
        let restorable = client
            .list_restorable_contacts_json(
                &json!({"schema_version": 1, "book_id": BOOK}).to_string(),
            )
            .unwrap();
        no_secret(&restorable);
        let restorable: Value = serde_json::from_str(&restorable).unwrap();
        assert_eq!(
            restorable["contacts"][0]["photo"]["attachment_id"],
            second.to_string()
        );
        assert!(!client.discard_unreferenced_attachment(second).unwrap());
    }
    assert_eq!(state(&p.owner)["reclaims"], json!([]));
}

#[test]
fn requester_photo_edit_reaches_owner_as_pending_download_without_key_in_ledger() {
    let mut p = pair();
    call(capture(&p.owner, &p.owner_cfg, None));
    p.deliver(true);
    let revision = find(&p.peer, "c1").unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let id = p
        .peer
        .prepare_contact_photo(&photo_file(p.dir.path(), "edit.png", 42))
        .unwrap()
        .attachment_id;
    let request = json!({
        "schema_version": 1, "request_id": "r1", "target_owner": p.owner_cfg.device_id.to_string(),
        "book_id": BOOK, "kind": "update", "contact_id": "c1", "base_revision": revision,
        "photo_op": {"op": "set", "attachment_id": id.to_string()}
    });
    call(p.peer.request_contact_edit(&request.to_string()));
    assert!(
        p.peer.pending_outbox().unwrap().is_empty(),
        "held until upload"
    );
    assert_eq!(
        state(&p.peer)["uploads"][0]["attachment_id"],
        id.to_string()
    );
    assert!(!p.peer.discard_unreferenced_attachment(id).unwrap());
    let remote = p.upload_and_register(&p.peer, id);
    p.deliver(false);

    // No OS write authorization before the photo is downloaded and verified locally.
    let permit_input = json!({"schema_version": 1, "request_id": "r1"}).to_string();
    for _ in 0..2 {
        let waiting = call(p.owner.next_contact_apply_permit(&permit_input));
        assert_eq!(
            waiting,
            json!({"status": "waiting_media", "request_id": "r1", "attachment_id": id.to_string()})
        );
    }
    let download = p.owner.pending_downloads().unwrap();
    assert_eq!(download.len(), 1);
    assert_eq!(
        download[0].remote_object_id.as_deref(),
        Some(remote.as_str())
    );
    let fetched = p.dir.path().join("edit.opss");
    std::fs::copy(p.peer.native_cipher_file(id).unwrap(), &fetched).unwrap();
    p.owner.install_downloaded_attachment(id, &fetched).unwrap();
    let permit = call(p.owner.next_contact_apply_permit(&permit_input));
    assert_eq!(permit["status"], "permit");
    assert_eq!(
        permit["request"]["photo_op"],
        json!({"op": "set", "attachment_id": id.to_string()})
    );
    // The owner-only permit carries provider `source` by design, but never media secrets.
    no_media_secret(&permit.to_string());
    assert!(p.owner.pending_downloads().unwrap().is_empty());
    assert_eq!(plaintext(&p.owner, id), plaintext(&p.peer, id));
    assert!(!p.owner.discard_unreferenced_attachment(id).unwrap());
}

#[test]
fn inline_or_unknown_photo_references_are_rejected_at_capture() {
    let p = pair();
    let mut inline = contact(None);
    inline["photo"] = json!("data:image/jpeg;base64,/9j/4AAQ");
    let input = json!({"schema_version": 1, "book": book(&p.owner_cfg), "contacts": [inline]});
    assert!(matches!(
        p.owner.capture_contact_book(&input.to_string()),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        capture(&p.owner, &p.owner_cfg, Some(AttachmentId::new())),
        Err(Error::InvalidRequest(_))
    ));
    // A non-photo attachment cannot be used as a contact photo.
    let other = p.dir.path().join("doc.txt");
    std::fs::write(&other, b"hello").unwrap();
    let doc = p
        .owner
        .prepare_attachment(&other, "text/plain", "doc.txt")
        .unwrap()
        .attachment_id;
    assert!(matches!(
        capture(&p.owner, &p.owner_cfg, Some(doc)),
        Err(Error::InvalidRequest(_))
    ));
    assert!(p.owner.pending_outbox().unwrap().is_empty());
}

fn reclaim_completed(client: &Client, id: AttachmentId) {
    let listed = state(client)["reclaims"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["attachment_id"] == id.to_string());
    assert!(listed, "{id} is a reclaim candidate");
    let out = call(
        client.acknowledge_contact_photo_reclaim_json(
            &json!({"schema_version": 1, "attachment_id": id.to_string(), "http_status": 204})
                .to_string(),
        ),
    );
    assert_eq!(out["status"], "completed");
}

#[test]
fn released_photo_is_reprepared_and_never_reuses_its_dead_remote_object() {
    let mut p = pair();
    let first = p
        .owner
        .prepare_contact_photo(&photo_file(p.dir.path(), "a.png", 5))
        .unwrap()
        .attachment_id;
    call(capture(&p.owner, &p.owner_cfg, Some(first)));
    let dead_remote = p.upload_and_register(&p.owner, first);
    p.deliver(true);
    let original = plaintext(&p.owner, first);

    // Photo removed; the server proves release of the first object.
    call(capture(&p.owner, &p.owner_cfg, None));
    p.deliver(true);
    reclaim_completed(&p.owner, first);

    // A later use of the same local photo gets a fresh attachment and a fresh remote object.
    call(capture(&p.owner, &p.owner_cfg, Some(first)));
    let uploads = state(&p.owner)["uploads"].as_array().unwrap().clone();
    assert_eq!(uploads.len(), 1);
    let fresh: AttachmentId = uploads[0]["attachment_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(fresh, first);
    assert!(
        p.owner
            .pending_uploads()
            .unwrap()
            .iter()
            .any(|o| o.attachment_id == fresh)
    );
    // The upsert is held (unsealed) for the fresh upload: it is in no queued envelope yet.
    let before_upload = outbox_ids(&p.owner);
    assert_eq!(state(&p.owner)["registrations"], json!([]));
    assert_eq!(
        find(&p.owner, "c1").unwrap()["photo"]["attachment_id"],
        fresh.to_string()
    );
    let fresh_remote = remote_id();
    p.owner
        .mark_attachment_uploaded(fresh, &fresh_remote)
        .unwrap();
    let registrations = state(&p.owner)["registrations"].as_array().unwrap().clone();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0]["attachment_id"], fresh_remote);
    assert_eq!(registrations[0]["local_attachment_id"], fresh.to_string());
    assert!(!before_upload.contains(registrations[0]["envelope_id"].as_str().unwrap()));
    p.register_all(&p.owner);
    assert_ne!(fresh_remote, dead_remote);
    p.deliver(true);
    assert_eq!(
        find(&p.peer, "c1").unwrap()["photo"]["attachment_id"],
        fresh.to_string()
    );
    // The peer never fetched the first photo; it is now unreferenced there and discardable.
    let download = p.peer.pending_downloads().unwrap();
    let fresh_download = download.iter().find(|o| o.attachment_id == fresh).unwrap();
    assert_eq!(
        fresh_download.remote_object_id.as_deref(),
        Some(fresh_remote.as_str())
    );
    assert!(download.iter().all(
        |o| o.remote_object_id.as_deref() != Some(fresh_remote.as_str())
            || o.attachment_id == fresh
    ));
    assert!(p.peer.discard_unreferenced_attachment(first).unwrap());
    let fetched = p.dir.path().join("fresh.opss");
    std::fs::copy(p.owner.native_cipher_file(fresh).unwrap(), &fetched).unwrap();
    p.peer
        .install_downloaded_attachment(fresh, &fetched)
        .unwrap();
    assert_eq!(plaintext(&p.peer, fresh), original);
    assert!(!p.owner.discard_unreferenced_attachment(fresh).unwrap());

    // Released and without verified local bytes: an explicit error, nothing enqueued.
    let gone = p
        .owner
        .prepare_contact_photo(&photo_file(p.dir.path(), "g.png", 77))
        .unwrap()
        .attachment_id;
    call(capture(&p.owner, &p.owner_cfg, Some(gone)));
    p.upload_and_register(&p.owner, gone);
    p.deliver(true);
    call(capture(&p.owner, &p.owner_cfg, None));
    p.deliver(true);
    reclaim_completed(&p.owner, gone);
    std::fs::remove_file(p.owner.native_cipher_file(gone).unwrap()).unwrap();
    p.deliver(true);
    let err = capture(&p.owner, &p.owner_cfg, Some(gone)).unwrap_err();
    assert!(
        matches!(err, Error::InvalidRequest("contact photo unavailable")),
        "{err:?}"
    );
    assert!(p.owner.pending_outbox().unwrap().is_empty());
    assert_eq!(state(&p.owner)["uploads"], json!([]));
}
