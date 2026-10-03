//! Compaction frontier: legacy history, overflow folding, multiwriter filters and edit-request
//! lifecycles are fully referenced, so terminal-root expiry never strands an older state.
//! The end-to-end purge runs against the real server in services/server/tests/transport.rs.
mod common;
use common::*;
use serde_json::{Value, json};
use std::collections::HashSet;
use tempfile::TempDir;

type Identity = (DeviceId, u64);

fn id(envelope: &Envelope) -> Identity {
    (envelope.producer_device_id, envelope.producer_sequence.0)
}
fn refs(envelope: &Envelope) -> Vec<Identity> {
    envelope
        .compaction
        .as_ref()
        .map(|c| {
            c.supersedes
                .iter()
                .map(|r| (r.producer_device_id, r.producer_sequence.0))
                .collect()
        })
        .unwrap_or_default()
}

fn notification(text: &str) -> NotificationCapture {
    NotificationCapture {
        notification_key: "progress".into(),
        instance: "i".into(),
        package_name: "com.example.app".into(),
        app_name: "App".into(),
        title: "Download".into(),
        text: text.into(),
        category: None,
        posted_at: 1,
        dismissible: true,
    }
}

/// A pre-upgrade phone: unlocked, but no compaction-capable server recorded yet.
fn legacy_unlocked(config: &ClientConfig, vault: &Vault) -> Client {
    let client = open(config);
    client
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();
    client
}

fn readiness(client: &Client) -> Value {
    serde_json::from_str(&client.contact_sync_readiness_json().unwrap()).unwrap()
}

#[test]
fn upgraded_legacy_lifetime_is_fully_superseded_and_never_resurrects_after_expiry() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let phone_cfg = config(&dir, "phone", &vault);
    let phone = legacy_unlocked(&phone_cfg, &vault);
    let mut server = Server::default();
    assert_eq!(readiness(&phone)["state"], "server_unsupported");
    for text in ["10%", "50%", "90%"] {
        assert_eq!(
            phone.capture_notification(notification(text)).unwrap(),
            NotificationCaptureOutcome::Captured
        );
    }
    assert_eq!(server.upload(&phone), 3);
    assert!(
        server.log.iter().all(|e| e.compaction.is_none()),
        "pre-upgrade posts are legacy"
    );

    // Upgrade: the host handshake records support and backfills the frontier.
    connect(&phone);
    assert_eq!(readiness(&phone)["state"], "ready");
    phone.remove_notification("progress", "i").unwrap();
    assert_eq!(server.upload(&phone), 1);
    let removal = server.log.last().unwrap().clone();
    let compaction = removal.compaction.as_ref().unwrap();
    assert!(compaction.terminal);
    let legacy: HashSet<Identity> = server.log[..3].iter().map(id).collect();
    assert_eq!(
        refs(&removal).into_iter().collect::<HashSet<_>>(),
        legacy,
        "every legacy post, not only the latest"
    );

    // The real server purge and fresh fenced import of this exact history are exercised by
    // services/server/tests/transport.rs::real_client_history_purges_on_the_real_server_*.
}

#[test]
fn oversized_legacy_frontier_folds_into_authenticated_checkpoints() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let phone = legacy_unlocked(&config(&dir, "phone", &vault), &vault);
    let mut server = Server::default();
    for step in 0..130 {
        phone
            .capture_notification(notification(&format!("{step}")))
            .unwrap();
    }
    assert_eq!(server.upload(&phone), 130);
    connect(&phone);
    phone.remove_notification("progress", "i").unwrap();
    let uploaded = server.upload(&phone);
    assert_eq!(uploaded, 2, "one removal plus one checkpoint");
    let (main, checkpoint) = (&server.log[130], &server.log[131]);
    assert!(!main.compaction.as_ref().unwrap().checkpoint);
    let marker = checkpoint.compaction.as_ref().unwrap();
    assert!(marker.checkpoint && marker.terminal);
    assert_eq!(refs(main).len(), 128);
    assert!(
        refs(checkpoint).contains(&id(main)),
        "checkpoint closes the frontier"
    );
    let covered: HashSet<Identity> = refs(main).into_iter().chain(refs(checkpoint)).collect();
    assert!(server.log[..130].iter().all(|e| covered.contains(&id(e))));

    // Readers apply the checkpoint as history: the removal state is unchanged, nothing new shows.
    let desktop = unlocked(&config(&dir, "desktop", &vault), &vault);
    assert_eq!(server.sync(&desktop).quarantined, 0);
    while desktop.apply_pending(100).unwrap().applied > 0 {}
    assert!(
        desktop
            .notification_snapshot()
            .unwrap()
            .notifications
            .is_empty()
    );
    assert!(desktop.pending_banner_candidates(10).unwrap().is_empty());
}

#[test]
fn out_of_order_app_filters_are_all_superseded_by_the_next_write() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let a = unlocked(&config(&dir, "a", &vault), &vault);
    let b = unlocked(&config(&dir, "b", &vault), &vault);
    let c = unlocked(&config(&dir, "c", &vault), &vault);
    let source = DeviceId::new().to_string();
    a.set_app_muted(&source, "pkg", "App", true).unwrap();
    b.set_app_muted(&source, "pkg", "App", false).unwrap();
    let (mut from_a, mut from_b) = (Server::default(), Server::default());
    from_a.upload(&a);
    from_b.upload(&b);
    // C receives B's write first and A's concurrent write later.
    let mut late = Server {
        log: vec![from_b.log[0].clone(), from_a.log[0].clone()],
    };
    assert_eq!(late.sync(&c).quarantined, 0);
    c.set_app_muted(&source, "pkg", "App", true).unwrap();
    late.upload(&c);
    let write = late.log.last().unwrap();
    let expected: HashSet<Identity> = [id(&from_a.log[0]), id(&from_b.log[0])].into();
    assert_eq!(refs(write).into_iter().collect::<HashSet<_>>(), expected);
    assert!(
        !write.compaction.as_ref().unwrap().terminal,
        "latest filter state never expires"
    );
}

const BOOK: &str = "book1";

#[test]
fn edit_request_lifecycle_closes_intermediate_results_and_expires() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let owner_cfg = config(&dir, "owner", &vault);
    let owner = unlocked(&owner_cfg, &vault);
    let peer = unlocked(&config(&dir, "peer", &vault), &vault);
    let mut server = Server::default();
    let book = json!({"id": BOOK, "owner_device_id": owner_cfg.device_id.to_string(), "generation": "1", "state": "active",
        "capabilities": {"read": true, "write": true, "photo": true}, "policy": {"remote_edits": "confirm"}});
    let contact = json!({"id": "c1", "book_id": BOOK, "display_name": "Ada Lovelace", "name": {"given": "Ada", "family": "Lovelace"},
        "phones": [], "emails": [], "addresses": []});
    owner
        .capture_contact_book(
            &json!({"schema_version": 1, "book": book, "contacts": [contact]}).to_string(),
        )
        .unwrap();
    server.upload(&owner);
    server.sync(&peer);
    let request = json!({"schema_version": 1, "request_id": "r1", "target_owner": owner_cfg.device_id.to_string(),
        "book_id": BOOK, "kind": "update", "contact_id": "c1", "base_revision": "1",
        "expires_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 86_400,
        "patches": [{"op": "replace", "path": "name.given", "value": "Augusta"}]});
    peer.request_contact_edit(&request.to_string()).unwrap();
    let before = server.log.len();
    server.upload(&peer);
    server.sync(&owner);
    let permit = |client: &Client| -> Value {
        serde_json::from_str(
            &client
                .next_contact_apply_permit(
                    &json!({"schema_version": 1, "request_id": "r1"}).to_string(),
                )
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(permit(&owner)["status"], "awaiting_approval");
    owner
        .contact_approval_json(
            &json!({"schema_version": 1, "request_id": "r1", "approve": true}).to_string(),
        )
        .unwrap();
    assert_eq!(permit(&owner)["status"], "permit");
    let reconcile = |outcome: &str| {
        owner
            .reconcile_contact_apply(
                &json!({"schema_version": 1, "request_id": "r1", "outcome": outcome}).to_string(),
            )
            .unwrap();
    };
    reconcile("unknown");
    reconcile("failed");
    server.upload(&owner);
    let request_record = server.log[before].clone();
    assert_eq!(
        request_record.producer_device_id,
        peer_device(&server, &owner_cfg, before)
    );
    assert!(
        request_record.compaction.as_ref().unwrap().terminal,
        "requests expire after the action window"
    );
    // Results share one grouping key; the first one references the request.
    let owner_records: Vec<&Envelope> = server.log[before + 1..]
        .iter()
        .filter(|e| e.producer_device_id == owner_cfg.device_id)
        .collect();
    let first = owner_records
        .iter()
        .find(|e| refs(e) == vec![id(&request_record)])
        .expect("awaiting result");
    let result_key = first.compaction.as_ref().unwrap().key.clone();
    let results: Vec<&Envelope> = owner_records
        .into_iter()
        .filter(|e| e.compaction.as_ref().is_some_and(|c| c.key == result_key))
        .collect();
    assert!(
        results.len() >= 3,
        "awaiting, outcome_unknown, failed: {}",
        results.len()
    );
    // Each result supersedes the previous lifecycle record; only the final one is terminal.
    let mut previous = id(&request_record);
    for (index, result) in results.iter().enumerate() {
        assert_eq!(refs(result), vec![previous], "result {index}");
        assert_eq!(
            result.compaction.as_ref().unwrap().terminal,
            index + 1 == results.len()
        );
        previous = id(result);
    }
}

fn peer_device(server: &Server, owner: &ClientConfig, index: usize) -> DeviceId {
    let device = server.log[index].producer_device_id;
    assert_ne!(device, owner.device_id);
    device
}
