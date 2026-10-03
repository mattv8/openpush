//! Compaction snapshots are authoritative for compactable state: a receiver that missed a
//! tombstone the server has since purged (offline past the 120-day horizon) loses the removed
//! contact/notification, while owner state, unsent local writes and non-compactable history are
//! untouched, and nothing visible is cleared before the whole snapshot is authenticated.
mod common;
use common::*;
use serde_json::{Value, json};
use std::collections::HashSet;
use tempfile::TempDir;

const BOOK: &str = "book1";

fn call(result: Result<String, Error>) -> Value {
    serde_json::from_str(&result.unwrap()).unwrap()
}
fn book(owner: &ClientConfig, id: &str) -> Value {
    json!({
        "id": id,
        "owner_device_id": owner.device_id.to_string(),
        "generation": "1",
        "state": "active",
        "capabilities": {"read": true, "write": true, "photo": true},
        "accounts": [{"id": "native:acct", "name": "Google", "writable": true}],
        "default_account_id": "native:acct",
        "policy": {"remote_edits": "auto", "large_delete_requires_approval": true}
    })
}
fn contact(book_id: &str, id: &str, given: &str) -> Value {
    json!({
        "id": id,
        "book_id": book_id,
        "display_name": format!("{given} Lovelace"),
        "name": {"given": given, "family": "Lovelace"},
        "phones": [{"id": "p1", "label": "mobile", "value": "+12025550100"}],
        "emails": [],
        "addresses": [],
        "provenance": {"source_id": format!("native:src-{id}"), "account_id": "native:acct", "read_only": false}
    })
}
fn capture(client: &Client, book: &Value, contacts: Vec<Value>) {
    call(client.capture_contact_book(
        &json!({"schema_version": 1, "book": book, "contacts": contacts}).to_string(),
    ));
}
/// Visible (non-tombstoned) contact ids of `book_id`; empty when the book is unknown.
fn visible(client: &Client, book_id: &str) -> Vec<String> {
    let Ok(raw) = client.contact_book_view(
        &json!({"schema_version": 1, "book_id": book_id, "limit": 200}).to_string(),
    ) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = serde_json::from_str::<Value>(&raw).unwrap()["contacts"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|c| c["id"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}
/// The owner removes `id` with an authoritative full scan observing only `keep`.
fn tombstone(owner: &Client, book_id: &str, scan: &str, keep: &[&str]) {
    call(owner.begin_contact_scan(
        &json!({"schema_version": 1, "scan_id": scan, "book_id": book_id, "generation": "1", "access": "full", "authoritative": true}).to_string(),
    ));
    for id in keep {
        owner
            .observe_contact_scan(
                &json!({"schema_version": 1, "scan_id": scan, "contact": contact(book_id, id, "Kept")}).to_string(),
            )
            .unwrap();
    }
    let done = call(owner.finish_contact_scan(
        &json!({"schema_version": 1, "scan_id": scan, "complete": true}).to_string(),
    ));
    assert_eq!(done["deletions_applied"], true, "{done}");
}

/// Server compaction model: a record named in any retained record's `supersedes` is dropped,
/// and (past the purge horizon) terminal tombstones are dropped too. Cursors are preserved.
fn compact(log: &[Envelope], purge_terminal: bool) -> Vec<SnapshotRecord> {
    let superseded: HashSet<(DeviceId, u64)> = log
        .iter()
        .filter_map(|e| e.compaction.as_ref())
        .flat_map(|c| c.supersedes.iter())
        .map(|r| (r.producer_device_id, r.producer_sequence.0))
        .collect();
    log.iter()
        .enumerate()
        .filter(|(_, e)| !superseded.contains(&(e.producer_device_id, e.producer_sequence.0)))
        .filter(|(_, e)| !(purge_terminal && e.compaction.as_ref().is_some_and(|c| c.terminal)))
        .map(|(i, e)| SnapshotRecord {
            cursor: Cursor(i as u64 + 1),
            envelope: e.clone(),
        })
        .collect()
}
fn import(client: &Client, records: &[SnapshotRecord], high_water: usize, generation: Option<u64>) {
    let session = client
        .begin_snapshot_with_compaction(
            Cursor(high_water as u64),
            records.len() as u64,
            SnapshotPurpose::Resync,
            generation,
        )
        .unwrap();
    for page in records.chunks(2) {
        client
            .append_snapshot_page(session.generation, page)
            .unwrap();
    }
    client.finish_snapshot(session.generation).unwrap();
}
/// Calls `apply_pending` like native hosts do until no step reports progress.
fn settle(client: &Client) -> ApplyReport {
    let mut total = ApplyReport::default();
    loop {
        let step = client.apply_pending(1000).unwrap();
        total.applied += step.applied;
        total.quarantined += step.quarantined;
        total.drained += step.drained;
        total.superseded += step.superseded;
        if step.applied + step.drained + step.quarantined + step.superseded == 0
            && step.snapshot_remaining == 0
        {
            return total;
        }
    }
}
fn state(client: &Client) -> Option<(SnapshotProjectionState, Option<String>)> {
    client
        .snapshot_projection_status()
        .unwrap()
        .map(|s| (s.state, s.reason))
}

struct Pair {
    _dir: TempDir,
    vault: Vault,
    owner_cfg: ClientConfig,
    peer_cfg: ClientConfig,
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
    for client in [&owner, &peer] {
        client.set_server_compaction_supported(true).unwrap();
    }
    Pair {
        _dir: dir,
        vault,
        owner_cfg,
        peer_cfg,
        owner,
        peer,
        server: Server::default(),
    }
}

#[test]
fn offline_receiver_loses_purged_contact_only_after_complete_compaction_snapshot() {
    let mut p = pair();
    let b = book(&p.owner_cfg, BOOK);
    capture(
        &p.owner,
        &b,
        vec![contact(BOOK, "c1", "Ada"), contact(BOOK, "c2", "Bea")],
    );
    // A separate receiver whose process we can crash and reopen.
    let offline_cfg = config(&p._dir, "offline", &p.vault);
    let offline = unlocked(&offline_cfg, &p.vault);
    p.server.upload(&p.owner);
    p.server.sync(&offline);
    assert_eq!(visible(&offline, BOOK), ["c1", "c2"]);

    // The receiver goes offline; the owner removes c1 and the server later purges the
    // tombstone together with the upsert it superseded.
    tombstone(&p.owner, BOOK, "scan1", &["c2"]);
    p.server.upload(&p.owner);
    let high_water = p.server.log.len();
    let retained = compact(&p.server.log, true);
    assert!(retained.len() < high_water, "compaction must drop records");
    assert!(retained.iter().all(|r| r.envelope.compaction.is_some()));

    // A fresh database needs no repair; a reported one latches until promotion.
    assert!(!offline.contact_repair_required().unwrap());
    offline.request_contact_repair().unwrap();
    assert!(offline.contact_repair_required().unwrap());

    // Partial progress never clears anything.
    import(&offline, &retained, high_water, Some(7));
    let first = offline.apply_pending(1).unwrap();
    assert!(first.drained > 0 || first.snapshot_remaining > 0);
    assert_eq!(
        visible(&offline, BOOK),
        ["c1", "c2"],
        "partial import must not clear"
    );
    assert!(matches!(
        state(&offline),
        Some((
            SnapshotProjectionState::Draining | SnapshotProjectionState::Staging,
            None
        ))
    ));

    // Crash and reopen mid-import: the durable stage resumes.
    drop(offline);
    let offline = unlocked(&offline_cfg, &p.vault);
    assert_eq!(visible(&offline, BOOK), ["c1", "c2"]);
    settle(&offline);
    assert_eq!(
        state(&offline),
        Some((SnapshotProjectionState::Promoted, None))
    );
    assert_eq!(
        visible(&offline, BOOK),
        ["c2"],
        "purged contact must disappear"
    );
    assert!(
        !offline.contact_repair_required().unwrap(),
        "promotion clears the repair latch"
    );
    assert_eq!(offline.quarantined().unwrap(), vec![]);

    // The owner importing the same snapshot keeps its own state and can still rescan.
    import(&p.owner, &retained, high_water, Some(7));
    settle(&p.owner);
    assert_eq!(
        state(&p.owner),
        Some((SnapshotProjectionState::Promoted, None))
    );
    assert_eq!(visible(&p.owner, BOOK), ["c2"]);
    capture(
        &p.owner,
        &b,
        vec![contact(BOOK, "c2", "Kept"), contact(BOOK, "c3", "Cy")],
    );
    assert_eq!(visible(&p.owner, BOOK), ["c2", "c3"]);

    // Live sync continues on top of the promoted snapshot.
    p.server.upload(&p.owner);
    p.server.sync(&offline);
    assert_eq!(visible(&offline, BOOK), ["c2", "c3"]);
}

#[test]
fn legacy_snapshot_merges_and_keeps_removed_contact() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, BOOK),
        vec![contact(BOOK, "c1", "Ada"), contact(BOOK, "c2", "Bea")],
    );
    p.server.upload(&p.owner);
    p.server.sync(&p.peer);
    tombstone(&p.owner, BOOK, "scan1", &["c2"]);
    p.server.upload(&p.owner);
    let high_water = p.server.log.len();
    import(&p.peer, &compact(&p.server.log, true), high_water, None);
    settle(&p.peer);
    assert_eq!(state(&p.peer), None);
    assert_eq!(
        visible(&p.peer, BOOK),
        ["c1", "c2"],
        "legacy merge cannot remove"
    );
}

#[test]
fn pending_superseded_upsert_cannot_resurrect_and_journaled_records_reconstruct() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, BOOK),
        vec![contact(BOOK, "c1", "Ada"), contact(BOOK, "c2", "Bea")],
    );
    p.server.upload(&p.owner);
    // A locked receiver journals the upserts but cannot apply them.
    let locked_cfg = config(&p._dir, "locked", &p.vault);
    let locked = open(&locked_cfg);
    p.server.sync(&locked);
    assert!(visible(&locked, BOOK).is_empty());

    tombstone(&p.owner, BOOK, "scan1", &["c2"]);
    p.server.upload(&p.owner);
    let high_water = p.server.log.len();
    import(&locked, &compact(&p.server.log, true), high_water, Some(3));
    // Locked: the drain completes, staging waits for keys without reporting busy work.
    let report = locked.apply_pending(1000).unwrap();
    assert_eq!((report.applied, report.snapshot_remaining), (0, 0));
    assert_eq!(
        state(&locked),
        Some((SnapshotProjectionState::Staging, None))
    );
    drop(locked);

    let unlocked_client = unlocked(&locked_cfg, &p.vault);
    let total = settle(&unlocked_client);
    assert_eq!(
        state(&unlocked_client),
        Some((SnapshotProjectionState::Promoted, None))
    );
    assert!(
        total.superseded > 0,
        "the pending c1 upsert must be suppressed"
    );
    assert_eq!(visible(&unlocked_client, BOOK), ["c2"]);
    // Re-applying never revives it either.
    settle(&unlocked_client);
    assert_eq!(visible(&unlocked_client, BOOK), ["c2"]);
}

#[test]
fn unauthenticated_retained_record_fails_projection_and_keeps_view() {
    let mut p = pair();
    let b = book(&p.owner_cfg, BOOK);
    capture(
        &p.owner,
        &b,
        vec![contact(BOOK, "c1", "Ada"), contact(BOOK, "c2", "Bea")],
    );
    p.server.upload(&p.owner);
    p.server.sync(&p.peer);
    tombstone(&p.owner, BOOK, "scan1", &["c2"]);
    capture(
        &p.owner,
        &b,
        vec![contact(BOOK, "c2", "Kept"), contact(BOOK, "c3", "Cy")],
    );
    p.server.upload(&p.owner);
    let high_water = p.server.log.len();
    let mut retained = compact(&p.server.log, true);
    // Corrupt one retained record the peer has not journaled yet.
    let last = retained.last_mut().unwrap();
    let byte = last.envelope.ciphertext.last_mut().unwrap();
    *byte ^= 0x55;

    p.peer.request_contact_repair().unwrap();
    import(&p.peer, &retained, high_water, Some(9));
    settle(&p.peer);
    assert!(
        p.peer.contact_repair_required().unwrap(),
        "a failed projection keeps the repair latch"
    );
    assert_eq!(
        state(&p.peer),
        Some((
            SnapshotProjectionState::Failed,
            Some("authentication_failed".into())
        ))
    );
    // Previous visible state is kept; the snapshot never became authoritative.
    assert!(visible(&p.peer, BOOK).contains(&"c1".to_owned()));

    // A later valid snapshot still promotes.
    let retained = compact(&p.server.log, true);
    let fresh_cfg = config(&p._dir, "fresh", &p.vault);
    let fresh = unlocked(&fresh_cfg, &p.vault);
    import(&fresh, &retained, high_water, Some(10));
    settle(&fresh);
    assert_eq!(
        state(&fresh),
        Some((SnapshotProjectionState::Promoted, None))
    );
    assert_eq!(visible(&fresh, BOOK), ["c2", "c3"]);
}

#[test]
fn zero_record_compaction_snapshot_clears_remote_state_only() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, BOOK),
        vec![contact(BOOK, "c1", "Ada")],
    );
    capture(
        &p.peer,
        &book(&p.peer_cfg, "own"),
        vec![contact("own", "m1", "Me")],
    );
    p.server.upload(&p.owner);
    p.server.upload(&p.peer);
    p.server.sync(&p.peer);
    assert_eq!(visible(&p.peer, BOOK), ["c1"]);
    let high_water = p.server.log.len();
    import(&p.peer, &[], high_water, Some(4));
    settle(&p.peer);
    assert_eq!(
        state(&p.peer),
        Some((SnapshotProjectionState::Promoted, None))
    );
    assert!(visible(&p.peer, BOOK).is_empty());
    assert_eq!(visible(&p.peer, "own"), ["m1"], "owned book is local truth");
}

#[test]
fn historical_snapshot_edit_request_never_yields_a_permit() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, BOOK),
        vec![contact(BOOK, "c1", "Ada")],
    );
    p.server.upload(&p.owner);
    p.server.sync(&p.peer);
    let request = json!({
        "schema_version": 1,
        "request_id": "r1",
        "target_owner": p.owner_cfg.device_id.to_string(),
        "book_id": BOOK,
        "kind": "update",
        "contact_id": "c1",
        "base_revision": "1",
        "expires_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 86_400,
        "patches": [{"op": "replace", "path": "name.given", "value": "Augusta"}]
    });
    call(p.peer.request_contact_edit(&request.to_string()));
    p.server.upload(&p.peer);
    // The owner was offline and learns the request only from the compaction snapshot.
    let high_water = p.server.log.len();
    import(&p.owner, &compact(&p.server.log, true), high_water, Some(5));
    settle(&p.owner);
    assert_eq!(
        state(&p.owner),
        Some((SnapshotProjectionState::Promoted, None))
    );
    let permit = p
        .owner
        .next_contact_apply_permit(&json!({"schema_version": 1, "request_id": "r1"}).to_string());
    if let Ok(permit) = permit {
        assert_ne!(
            serde_json::from_str::<Value>(&permit).unwrap()["status"],
            "permit",
            "historical request must not authorize an OS write"
        );
    }
    assert_eq!(visible(&p.owner, BOOK), ["c1"]);
}

fn notification(key: &str, package: &str, text: &str) -> NotificationCapture {
    NotificationCapture {
        notification_key: key.into(),
        instance: format!("{key}-1"),
        package_name: package.into(),
        app_name: "App".into(),
        title: "Title".into(),
        text: text.into(),
        category: None,
        posted_at: 42,
        dismissible: true,
    }
}
fn shown(client: &Client) -> Vec<String> {
    let mut keys: Vec<String> = client
        .notification_snapshot()
        .unwrap()
        .notifications
        .into_iter()
        .map(|n| n.target.notification_key)
        .collect();
    keys.sort();
    keys
}
fn muted(client: &Client, package: &str) -> Option<bool> {
    client
        .notification_snapshot()
        .unwrap()
        .app_filters
        .into_iter()
        .find(|f| f.package_name == package)
        .map(|f| f.muted)
}

#[test]
fn purged_notification_disappears_and_filters_keep_lww_and_unsent_writes() {
    let mut p = pair();
    let (phone, desktop) = (&p.owner, &p.peer);
    let phone_id = p.owner_cfg.device_id.to_string();
    for (key, text) in [("n1", "first"), ("n2", "second")] {
        phone
            .capture_notification(notification(key, "com.example.mail", text))
            .unwrap();
    }
    phone
        .capture_notification(notification("n3", "com.example.news", "headline"))
        .unwrap();
    p.server.upload(phone);
    p.server.sync(desktop);
    assert_eq!(shown(desktop), ["n1", "n2", "n3"]);

    // Desktop offline: phone removes n1 and mutes news; the desktop mutes chat locally but
    // has not uploaded that write yet.
    phone.remove_notification("n1", "n1-1").unwrap();
    phone
        .set_app_muted(&phone_id, "com.example.news", "News", true)
        .unwrap();
    p.server.upload(phone);
    desktop
        .set_app_muted(&phone_id, "com.example.chat", "Chat", true)
        .unwrap();

    let high_water = p.server.log.len();
    let retained = compact(&p.server.log, true);
    import(desktop, &retained, high_water, Some(11));
    settle(desktop);
    assert_eq!(
        state(desktop),
        Some((SnapshotProjectionState::Promoted, None))
    );
    assert_eq!(shown(desktop), ["n2"], "purged n1 gone, muted news hidden");
    assert_eq!(muted(desktop, "com.example.news"), Some(true));
    assert_eq!(
        muted(desktop, "com.example.chat"),
        Some(true),
        "unsent local filter write is preserved"
    );
    // The phone's own notifications are local truth and untouched by its own import.
    import(phone, &retained, high_water, Some(11));
    settle(phone);
    assert_eq!(shown(phone), ["n2"]);

    // The unsent filter still reaches the server and the phone afterwards.
    p.server.upload(desktop);
    p.server.sync(phone);
    assert_eq!(muted(phone, "com.example.chat"), Some(true));
}

fn books(client: &Client) -> Value {
    serde_json::from_str(&client.list_contact_books_json().unwrap()).unwrap()
}

#[test]
fn snapshot_out_of_order_upsert_cannot_rewind_retired_book() {
    let p = pair();
    let b = book(&p.owner_cfg, BOOK);
    capture(&p.owner, &b, vec![contact(BOOK, "c1", "Ada")]);
    let mut retired = b.clone();
    retired["state"] = json!("retired");
    capture(&p.owner, &retired, vec![]);
    // The photo-held upsert lands after the retirement.
    let mut pending = p.owner.pending_outbox().unwrap();
    pending.sort_by_key(|e| std::cmp::Reverse(e.producer_sequence.0));
    let records = pending
        .iter()
        .enumerate()
        .map(|(i, e)| SnapshotRecord {
            cursor: Cursor(i as u64 + 1),
            envelope: e.clone(),
        })
        .collect::<Vec<_>>();
    import(&p.peer, &records, records.len(), Some(3));
    settle(&p.peer);
    assert_eq!(
        state(&p.peer),
        Some((SnapshotProjectionState::Promoted, None))
    );
    assert_eq!(books(&p.peer)["books"][0]["state"], "retired");
    assert_eq!(visible(&p.peer, BOOK), ["c1"]);
}

#[test]
fn snapshot_tombstone_before_book_state_keeps_restore_data() {
    let p = pair();
    let b = book(&p.owner_cfg, BOOK);
    capture(
        &p.owner,
        &b,
        vec![contact(BOOK, "c1", "Ada"), contact(BOOK, "c2", "Bea")],
    );
    for e in p.owner.pending_outbox().unwrap() {
        p.owner.ack_outbox(e.envelope_id).unwrap();
    }
    tombstone(&p.owner, BOOK, "scan1", &["c2"]);
    // Compacted: c2's upsert and the book-bearing records are gone except the removal and
    // the latest book state, which follows it.
    let records = p
        .owner
        .pending_outbox()
        .unwrap()
        .iter()
        .filter(|e| e.compaction.as_ref().is_some_and(|c| c.terminal) || true)
        .enumerate()
        .map(|(i, e)| SnapshotRecord {
            cursor: Cursor(i as u64 + 1),
            envelope: e.clone(),
        })
        .collect::<Vec<_>>();
    import(&p.peer, &records, records.len(), Some(3));
    settle(&p.peer);
    assert_eq!(
        state(&p.peer),
        Some((SnapshotProjectionState::Promoted, None))
    );
    let restorable = call(
        p.peer
            .list_restorable_contacts_json(&json!({"book_id": BOOK}).to_string()),
    );
    assert_eq!(restorable["contacts"][0]["id"], "c1");
}
