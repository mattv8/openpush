//! Two-client contact sync through the real sealed outbox: capture -> encrypted peer
//! projection -> remote edit request -> owner permit -> reconcile -> requester result.
//! IDs, revisions and generations are canonical strings, as in the DTO contract.

mod common;
use common::*;
use serde_json::{Value, json};
use tempfile::TempDir;

const BOOK: &str = "book1";

fn ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn call(result: Result<String, Error>) -> Value {
    serde_json::from_str(&result.unwrap()).unwrap()
}
fn book(owner: &ClientConfig, generation: &str, mode: &str) -> Value {
    json!({
        "id": BOOK,
        "owner_device_id": owner.device_id.to_string(),
        "generation": generation,
        "state": "active",
        "capabilities": {"read": true, "write": true, "photo": true},
        "accounts": [{"id": "native:acct-secret", "name": "Google", "writable": true}],
        "default_account_id": "native:acct-secret",
        "policy": {"remote_edits": mode, "large_delete_requires_approval": true}
    })
}
fn contact(id: &str, given: &str) -> Value {
    json!({
        "id": id,
        "book_id": BOOK,
        "revision": "999",
        "display_name": format!("{given} Lovelace"),
        "name": {"given": given, "family": "Lovelace"},
        "phones": [{"id": "p1", "label": "mobile", "value": "+12025550100"}],
        "emails": [],
        "addresses": [],
        "provenance": {"source_id": format!("native:src-{id}"), "account_id": "native:acct-secret", "read_only": false}
    })
}
fn capture(client: &Client, book: &Value, contacts: Vec<Value>) -> Value {
    call(client.capture_contact_book(
        &json!({"schema_version": 1, "book": book, "contacts": contacts}).to_string(),
    ))
}
fn view_raw(client: &Client) -> String {
    client
        .contact_book_view(&json!({"schema_version": 1, "book_id": BOOK, "limit": 200}).to_string())
        .unwrap()
}
fn contacts(client: &Client) -> Vec<Value> {
    serde_json::from_str::<Value>(&view_raw(client)).unwrap()["contacts"]
        .as_array()
        .unwrap()
        .clone()
}
fn find(client: &Client, id: &str) -> Option<Value> {
    contacts(client).into_iter().find(|c| c["id"] == id)
}
fn begin(client: &Client, scan: &str, generation: &str, access: &str, authoritative: bool) {
    call(client.begin_contact_scan(
        &json!({"schema_version": 1, "scan_id": scan, "book_id": BOOK, "generation": generation, "access": access, "authoritative": authoritative}).to_string(),
    ));
}
fn observe(client: &Client, scan: &str, contact: Value) -> Result<String, Error> {
    client.observe_contact_scan(
        &json!({"schema_version": 1, "scan_id": scan, "contact": contact}).to_string(),
    )
}
fn finish(client: &Client, scan: &str, complete: bool) -> Value {
    call(client.finish_contact_scan(
        &json!({"schema_version": 1, "scan_id": scan, "complete": complete}).to_string(),
    ))
}
fn request(owner: &ClientConfig, id: &str, kind: &str, base: &str, extra: Value) -> Value {
    let mut r = json!({
        "schema_version": 1,
        "request_id": id,
        "target_owner": owner.device_id.to_string(),
        "book_id": BOOK,
        "kind": kind,
        "contact_id": "c1",
        "base_revision": base,
        "expires_at": ts() + 86_400
    });
    for (k, v) in extra.as_object().unwrap() {
        r[k] = v.clone();
    }
    r
}
fn given_patch(value: &str) -> Value {
    json!({"patches": [{"op": "replace", "path": "name.given", "value": value}]})
}
fn permit(client: &Client, id: &str) -> Value {
    call(
        client
            .next_contact_apply_permit(&json!({"schema_version": 1, "request_id": id}).to_string()),
    )
}
fn reconcile(
    client: &Client,
    id: &str,
    outcome: &str,
    observed: Option<Value>,
) -> Result<String, Error> {
    let mut input = json!({"schema_version": 1, "request_id": id, "outcome": outcome});
    if let Some(observed) = observed {
        input["observed"] = observed;
    }
    client.reconcile_contact_apply(&input.to_string())
}
fn requester_state(client: &Client, config: &ClientConfig, id: &str) -> String {
    let list = call(client.list_contact_requests_json(
        &json!({"schema_version": 1, "requester": config.device_id.to_string()}).to_string(),
    ));
    list["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["request_id"] == id)
        .map(|r| r["state"].as_str().unwrap().to_owned())
        .unwrap()
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
impl Pair {
    /// Uploads `from` and applies everything on `to`, asserting nothing was quarantined.
    fn deliver(&mut self, owner_to_peer: bool) -> ApplyReport {
        let (from, to) = if owner_to_peer {
            (&self.owner, &self.peer)
        } else {
            (&self.peer, &self.owner)
        };
        self.server.upload(from);
        let report = self.server.sync(to);
        assert_eq!(report.quarantined, 0, "unexpected quarantine");
        report
    }
}

#[test]
fn capture_reaches_peer_sanitized_with_core_minted_revisions() {
    let mut p = pair();
    let captured = capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "auto"),
        vec![contact("c1", "Ada")],
    );
    assert_eq!(captured["changed"], 1);
    // Native-supplied revision "999" is ignored; core mints "1".
    assert_eq!(find(&p.owner, "c1").unwrap()["revision"], "1");
    // Identical content does not mint a new revision.
    assert_eq!(
        capture(
            &p.owner,
            &book(&p.owner_cfg, "1", "auto"),
            vec![contact("c1", "Ada")]
        )["changed"],
        0
    );
    assert_eq!(find(&p.owner, "c1").unwrap()["revision"], "1");
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "auto"),
        vec![contact("c1", "Augusta")],
    );
    assert_eq!(find(&p.owner, "c1").unwrap()["revision"], "2");
    // A lower generation is rejected.
    assert!(p
        .owner
        .capture_contact_book(&json!({"schema_version": 1, "book": book(&p.owner_cfg, "0", "auto"), "contacts": []}).to_string())
        .is_err());

    p.deliver(true);
    let peer_view = view_raw(&p.peer);
    let peer_c1 = find(&p.peer, "c1").unwrap();
    assert_eq!(peer_c1["display_name"], "Augusta Lovelace");
    assert_eq!(peer_c1["revision"], "2");
    for raw in [peer_view, view_raw(&p.owner)] {
        assert!(!raw.contains("native:"), "source identifiers leaked: {raw}");
        assert!(!raw.contains("provenance"));
    }
}

#[test]
fn scan_inserts_new_updates_changed_and_tombstones_missing() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    capture(
        &p.owner,
        &b,
        vec![
            contact("c1", "Ada"),
            contact("c2", "Bea"),
            contact("c3", "Cy"),
        ],
    );
    p.deliver(true);

    begin(&p.owner, "scan1", "1", "full", true);
    observe(&p.owner, "scan1", contact("c1", "Ada")).unwrap();
    observe(&p.owner, "scan1", contact("c2", "Beatrice")).unwrap();
    let fresh = call(observe(&p.owner, "scan1", contact("c4", "Dee")));
    assert_eq!(fresh["changed"], true);
    let done = finish(&p.owner, "scan1", true);
    assert_eq!(done["deleted"], 1);
    assert_eq!(done["deletions_applied"], true);
    // Once-only: the scan cannot be finished again or observed into.
    assert_eq!(finish(&p.owner, "scan1", true)["already_finished"], true);
    assert!(observe(&p.owner, "scan1", contact("c5", "Eve")).is_err());

    {
        let client = &p.owner;
        assert_eq!(find(client, "c1").unwrap()["revision"], "1");
        assert_eq!(find(client, "c2").unwrap()["revision"], "2");
        assert_eq!(find(client, "c4").unwrap()["revision"], "1");
        assert!(find(client, "c3").is_none());
    }
    p.deliver(true);
    assert_eq!(
        find(&p.peer, "c2").unwrap()["display_name"],
        "Beatrice Lovelace"
    );
    assert_eq!(find(&p.peer, "c4").unwrap()["display_name"], "Dee Lovelace");
    assert!(
        find(&p.peer, "c3").is_none(),
        "tombstone must reach the peer"
    );
}

#[test]
fn partial_limited_stale_or_non_authoritative_scans_never_delete() {
    let p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "auto"),
        vec![contact("c1", "Ada"), contact("c2", "Bea")],
    );

    begin(&p.owner, "partial", "1", "full", true);
    observe(&p.owner, "partial", contact("c1", "Ada")).unwrap();
    assert_eq!(finish(&p.owner, "partial", false)["deleted"], 0);
    // A partial finish is terminal: a later "complete" finish cannot delete.
    let again = finish(&p.owner, "partial", true);
    assert_eq!(again["already_finished"], true);
    assert_eq!(again["deleted"], 0);

    for (scan, generation, access, authoritative) in [
        ("limited", "1", "limited", true),
        ("stale", "2", "full", true),
        ("unauthoritative", "1", "full", false),
    ] {
        begin(&p.owner, scan, generation, access, authoritative);
        assert_eq!(finish(&p.owner, scan, true)["deleted"], 0, "{scan}");
    }
    assert_eq!(contacts(&p.owner).len(), 2);
}

#[test]
fn scan_mass_disappearance_is_held() {
    let p = pair();
    let all = (0..100)
        .map(|i| contact(&format!("c{i}"), "Ada"))
        .collect::<Vec<_>>();
    capture(&p.owner, &book(&p.owner_cfg, "1", "auto"), all.clone());
    begin(&p.owner, "scan1", "1", "full", true);
    for c in all.into_iter().take(50) {
        observe(&p.owner, "scan1", c).unwrap();
    }
    let done = finish(&p.owner, "scan1", true);
    assert_eq!(done["deleted"], 0);
    assert_eq!(done["held"], 50);
    assert_eq!(done["state"], "awaiting_approval");
    assert_eq!(contacts(&p.owner).len(), 100);
}

#[test]
fn foreign_book_claims_quarantine_without_wedging_sync() {
    let mut p = pair();
    // The peer claims the same book id as its own before seeing the owner's book.
    let mut forged = book(&p.peer_cfg, "9", "auto");
    forged["id"] = json!(BOOK);
    capture(&p.peer, &forged, vec![contact("c1", "Mallory")]);
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "auto"),
        vec![contact("c1", "Ada")],
    );
    p.server.upload(&p.peer);
    p.server.upload(&p.owner);
    let report = p.server.sync(&p.owner);
    assert!(report.quarantined > 0);
    assert_eq!(
        find(&p.owner, "c1").unwrap()["display_name"],
        "Ada Lovelace"
    );

    // Later records still apply after the quarantine.
    let mut other = book(&p.peer_cfg, "1", "auto");
    other["id"] = json!("book2");
    let mut c = contact("x1", "Zed");
    c["book_id"] = json!("book2");
    capture(&p.peer, &other, vec![c]);
    p.server.upload(&p.peer);
    let report = p.server.sync(&p.owner);
    assert!(report.applied > 0);
    let book2 = call(
        p.owner
            .contact_book_view(&json!({"schema_version": 1, "book_id": "book2"}).to_string()),
    );
    assert_eq!(book2["contacts"][0]["display_name"], "Zed Lovelace");
}

#[test]
fn auto_edit_roundtrip_permit_once_reconcile_and_result() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "auto"),
        vec![contact("c1", "Ada")],
    );
    p.deliver(true);

    let r = request(&p.owner_cfg, "r1", "update", "1", given_patch("Augusta"));
    call(p.peer.request_contact_edit(&r.to_string()));
    assert_eq!(requester_state(&p.peer, &p.peer_cfg, "r1"), "requested");
    p.deliver(false);

    let granted = permit(&p.owner, "r1");
    assert_eq!(granted["status"], "permit");
    assert_eq!(
        granted["source"]["source_key"], "native:src-c1",
        "native permit carries the source"
    );
    assert_eq!(
        granted["source"]["provenance"]["account_id"],
        "native:acct-secret"
    );
    assert_eq!(
        granted["book_source"]["default_account_id"],
        "native:acct-secret"
    );
    assert_eq!(granted["request"]["patches"][0]["value"], "Augusta");
    // A permit is issued once; asking again reopens as outcome_unknown.
    assert_eq!(permit(&p.owner, "r1")["status"], "outcome_unknown");
    // Applied requires observed evidence.
    assert!(reconcile(&p.owner, "r1", "applied", None).is_err());
    let applied = call(reconcile(
        &p.owner,
        "r1",
        "applied",
        Some(contact("c1", "Augusta")),
    ));
    assert_eq!(applied["status"], "applied");
    assert!(reconcile(&p.owner, "r1", "applied", Some(contact("c1", "Augusta"))).is_err());
    assert_eq!(find(&p.owner, "c1").unwrap()["revision"], "2");

    p.deliver(true);
    assert_eq!(
        find(&p.peer, "c1").unwrap()["display_name"],
        "Augusta Lovelace"
    );
    assert_eq!(requester_state(&p.peer, &p.peer_cfg, "r1"), "applied");
    assert!(!view_raw(&p.peer).contains("native:"));
}

#[test]
fn confirm_mode_waits_for_approval_and_unknown_is_never_applied() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "confirm"),
        vec![contact("c1", "Ada")],
    );
    p.deliver(true);
    call(p.peer.request_contact_edit(
        &request(&p.owner_cfg, "r1", "update", "1", given_patch("Augusta")).to_string(),
    ));
    p.deliver(false);

    assert_eq!(permit(&p.owner, "r1")["status"], "awaiting_approval");
    assert_eq!(permit(&p.owner, "r1")["status"], "awaiting_approval");
    // The awaiting state is published once to the requester.
    let report = p.deliver(true);
    assert_eq!(report.applied, 1);
    assert_eq!(
        requester_state(&p.peer, &p.peer_cfg, "r1"),
        "awaiting_approval"
    );
    call(p.owner.contact_approval_json(
        &json!({"schema_version": 1, "request_id": "r1", "approve": true}).to_string(),
    ));
    assert_eq!(permit(&p.owner, "r1")["status"], "permit");

    assert_eq!(
        call(reconcile(&p.owner, "r1", "unknown", None))["status"],
        "outcome_unknown"
    );
    let reopened = permit(&p.owner, "r1");
    assert_eq!(reopened["status"], "outcome_unknown");
    assert_eq!(reopened["request"]["request_id"], "r1");
    assert_eq!(find(&p.owner, "c1").unwrap()["revision"], "1");
    assert_eq!(
        call(reconcile(&p.owner, "r1", "failed", None))["status"],
        "failed"
    );
    p.deliver(true);
    assert_eq!(requester_state(&p.peer, &p.peer_cfg, "r1"), "failed");
    assert_eq!(find(&p.peer, "c1").unwrap()["display_name"], "Ada Lovelace");
}

#[test]
fn off_mode_rejects_and_requester_sees_the_result() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "off"),
        vec![contact("c1", "Ada")],
    );
    p.deliver(true);
    call(p.peer.request_contact_edit(
        &request(&p.owner_cfg, "r1", "update", "1", given_patch("Augusta")).to_string(),
    ));
    p.deliver(false);
    let decision = permit(&p.owner, "r1");
    assert_eq!(decision["status"], "rejected");
    assert_eq!(decision["reason"], "remote_edits_off");
    assert_eq!(permit(&p.owner, "r1")["status"], "rejected");
    p.deliver(true);
    assert_eq!(requester_state(&p.peer, &p.peer_cfg, "r1"), "rejected");
}

#[test]
fn stale_base_requires_matching_expected_old() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    capture(&p.owner, &b, vec![contact("c1", "Ada")]);
    let mut changed = contact("c1", "Ada");
    changed["display_name"] = json!("Countess");
    capture(&p.owner, &b, vec![changed]);
    p.deliver(true);

    let no_expected = request(&p.owner_cfg, "r1", "update", "1", given_patch("Augusta"));
    let mut matching = request(&p.owner_cfg, "r2", "update", "1", given_patch("Augusta"));
    matching["expected_old"] = json!({"name": {"given": "Ada"}});
    let mut mismatched = request(&p.owner_cfg, "r3", "update", "1", given_patch("Augusta"));
    mismatched["expected_old"] = json!({"name": {"given": "Bea"}});
    for r in [&no_expected, &matching, &mismatched] {
        call(p.peer.request_contact_edit(&r.to_string()));
    }
    p.deliver(false);
    assert_eq!(permit(&p.owner, "r1")["status"], "conflict");
    assert_eq!(permit(&p.owner, "r2")["status"], "permit");
    assert_eq!(permit(&p.owner, "r3")["status"], "conflict");
    p.deliver(true);
    assert_eq!(requester_state(&p.peer, &p.peer_cfg, "r1"), "conflict");
}

#[test]
fn remote_deletes_honor_the_rolling_cap() {
    let mut p = pair();
    let all = (0..30)
        .map(|i| contact(&format!("c{i}"), "Ada"))
        .collect::<Vec<_>>();
    capture(&p.owner, &book(&p.owner_cfg, "1", "auto"), all);
    p.deliver(true);
    let mut first = request(&p.owner_cfg, "d1", "delete", "1", json!({}));
    first["contact_id"] = json!("c1");
    let mut second = request(&p.owner_cfg, "d2", "delete", "1", json!({}));
    second["contact_id"] = json!("c2");
    call(p.peer.request_contact_edit(&first.to_string()));
    call(p.peer.request_contact_edit(&second.to_string()));
    p.deliver(false);

    assert_eq!(permit(&p.owner, "d1")["status"], "permit");
    assert_eq!(
        call(reconcile(&p.owner, "d1", "applied", None))["status"],
        "applied"
    );
    // 2 deletes in 24h of a 30-contact book exceeds 5%.
    assert_eq!(permit(&p.owner, "d2")["status"], "awaiting_approval");
    p.deliver(true);
    assert!(find(&p.peer, "c1").is_none());
    assert!(find(&p.peer, "c2").is_some());
    assert_eq!(requester_state(&p.peer, &p.peer_cfg, "d1"), "applied");
}

#[test]
fn expired_requests_are_recorded_and_published() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "auto"),
        vec![contact("c1", "Ada")],
    );
    p.deliver(true);
    let mut r = request(&p.owner_cfg, "r1", "update", "1", given_patch("Augusta"));
    r["expires_at"] = json!(ts() + 2);
    call(p.peer.request_contact_edit(&r.to_string()));
    p.deliver(false);
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_eq!(permit(&p.owner, "r1")["status"], "expired");
    assert_eq!(permit(&p.owner, "r1")["status"], "expired");
    p.deliver(true);
    assert_eq!(requester_state(&p.peer, &p.peer_cfg, "r1"), "expired");
}

#[test]
fn malformed_requests_fail_closed() {
    let p = pair();
    let unknown_op = request(
        &p.owner_cfg,
        "r1",
        "update",
        "1",
        json!({"patches": [{"op": "move", "path": "name.given", "value": "A"}]}),
    );
    assert!(
        p.peer
            .request_contact_edit(&unknown_op.to_string())
            .is_err()
    );
    let unknown_path = request(
        &p.owner_cfg,
        "r2",
        "update",
        "1",
        json!({"patches": [{"op": "replace", "path": "provenance.source_id", "value": "x"}]}),
    );
    assert!(
        p.peer
            .request_contact_edit(&unknown_path.to_string())
            .is_err()
    );
    let numeric_base = request(&p.owner_cfg, "r3", "update", "1", given_patch("A"));
    let mut numeric_base = numeric_base;
    numeric_base["base_revision"] = json!(1);
    assert!(
        p.peer
            .request_contact_edit(&numeric_base.to_string())
            .is_err()
    );
    let self_target = request(&p.peer_cfg, "r4", "update", "1", given_patch("A"));
    assert!(
        p.peer
            .request_contact_edit(&self_target.to_string())
            .is_err()
    );
}

fn approve(client: &Client, key: &str, id: &str, approve: bool) -> Value {
    call(client.contact_approval_json(
        &json!({"schema_version": 1, key: id, "approve": approve}).to_string(),
    ))
}

#[test]
fn owner_rejection_publishes_result_and_decisions_are_final() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "confirm"),
        vec![contact("c1", "Ada")],
    );
    p.deliver(true);
    call(p.peer.request_contact_edit(
        &request(&p.owner_cfg, "r1", "update", "1", given_patch("Augusta")).to_string(),
    ));
    p.deliver(false);
    assert_eq!(permit(&p.owner, "r1")["status"], "awaiting_approval");
    assert_eq!(
        approve(&p.owner, "request_id", "r1", false)["status"],
        "decided"
    );
    // Decided rows never change and never yield a permit.
    assert_eq!(
        approve(&p.owner, "request_id", "r1", true)["status"],
        "unchanged"
    );
    assert_eq!(permit(&p.owner, "r1")["status"], "rejected");
    p.deliver(true);
    assert_eq!(requester_state(&p.peer, &p.peer_cfg, "r1"), "rejected");
    // The requester cannot decide the owner's request.
    assert!(
        p.peer
            .contact_approval_json(
                &json!({"schema_version": 1, "request_id": "r1", "approve": true}).to_string()
            )
            .is_err()
    );
}

#[test]
fn held_scan_deletions_execute_once_after_approval() {
    let mut p = pair();
    let all = (0..40)
        .map(|i| contact(&format!("c{i:02}"), "Ada"))
        .collect::<Vec<_>>();
    let b = book(&p.owner_cfg, "1", "auto");
    capture(&p.owner, &b, all.clone());
    p.deliver(true);
    begin(&p.owner, "scan1", "1", "full", true);
    for c in all.iter().take(20) {
        observe(&p.owner, "scan1", c.clone()).unwrap();
    }
    let done = finish(&p.owner, "scan1", true);
    assert_eq!(done["held"], 20);
    assert_eq!(done["hold_id"], "scan1");
    // A held contact edited after the scan keeps its newer revision and is not removed.
    let mut edited = contact("c39", "Ada");
    edited["display_name"] = json!("Still Here");
    capture(&p.owner, &b, vec![edited]);

    let decided = approve(&p.owner, "scan_id", "scan1", true);
    assert_eq!(decided["state"], "applied");
    assert_eq!(decided["deleted"], 19);
    assert_eq!(
        approve(&p.owner, "scan_id", "scan1", true)["status"],
        "unchanged"
    );
    assert_eq!(contacts(&p.owner).len(), 21);
    p.deliver(true);
    assert_eq!(contacts(&p.peer).len(), 21);
    assert_eq!(find(&p.peer, "c39").unwrap()["display_name"], "Still Here");
}

#[test]
fn held_scan_deletions_are_stale_after_a_generation_change_or_rejection() {
    let p = pair();
    let all = (0..40)
        .map(|i| contact(&format!("c{i:02}"), "Ada"))
        .collect::<Vec<_>>();
    capture(&p.owner, &book(&p.owner_cfg, "1", "auto"), all.clone());
    let hold = |scan: &str, generation: &str| {
        begin(&p.owner, scan, generation, "full", true);
        for c in all.iter().take(20) {
            observe(&p.owner, scan, c.clone()).unwrap();
        }
        assert_eq!(finish(&p.owner, scan, true)["held"], 20);
    };
    hold("stale", "1");
    capture(&p.owner, &book(&p.owner_cfg, "2", "auto"), vec![]);
    let stale = approve(&p.owner, "scan_id", "stale", true);
    assert_eq!(stale["state"], "stale");
    assert_eq!(stale["deleted"], 0);
    hold("rejected", "2");
    assert_eq!(
        approve(&p.owner, "scan_id", "rejected", false)["state"],
        "rejected"
    );
    assert_eq!(contacts(&p.owner).len(), 40);
}

fn platform(key: &str, given: &str, phone_id: &str) -> Value {
    json!({
        "source_key": key,
        "fields": {
            "display_name": format!("{given} Lovelace"),
            "name": {"given": given},
            "phones": [{"id": phone_id, "label": "mobile", "value": "+12025550100"}],
            "emails": [],
            "addresses": []
        },
        "provenance": {"account_id": "native:acct-secret", "read_only": false}
    })
}

#[test]
fn platform_source_capture_edit_and_reconcile_share_one_mapping() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    let captured = call(p.owner.capture_platform_contacts_json(
        &json!({"schema_version": 1, "book": b, "contacts": [platform("native:raw-7", "Ada", "native:phone-9")]}).to_string(),
    ));
    let id = captured["contacts"][0]["id"].as_str().unwrap().to_owned();
    p.deliver(true);
    let peer_contact = find(&p.peer, &id).unwrap();
    let field_id = peer_contact["phones"][0]["id"].as_str().unwrap().to_owned();
    assert_ne!(field_id, "native:phone-9");
    assert!(
        !view_raw(&p.peer).contains("native:"),
        "{}",
        view_raw(&p.peer)
    );

    let mut r = request(
        &p.owner_cfg,
        "r1",
        "update",
        "1",
        json!({
            "patches": [{"op": "replace", "path": format!("phones[{field_id}]"), "value": {"id": field_id, "label": "work", "value": "+12025550199"}}]
        }),
    );
    r["contact_id"] = json!(id);
    call(p.peer.request_contact_edit(&r.to_string()));
    p.deliver(false);

    let granted = permit(&p.owner, "r1");
    assert_eq!(granted["status"], "permit");
    assert_eq!(granted["source"]["source_key"], "native:raw-7");
    assert_eq!(
        granted["source"]["field_sources"][0]["source_id"],
        "native:phone-9"
    );
    assert_eq!(granted["source"]["field_sources"][0]["id"], field_id);

    let mut written = platform("native:raw-7", "Ada", "native:phone-9");
    written["fields"]["phones"][0]["label"] = json!("work");
    written["fields"]["phones"][0]["value"] = json!("+12025550199");
    let applied = call(p.owner.reconcile_contact_apply(
        &json!({"schema_version": 1, "request_id": "r1", "outcome": "applied", "observed_source": written}).to_string(),
    ));
    assert_eq!(applied["status"], "applied");
    // The same provider key and item keep their core ids; a later scan echo is unchanged.
    begin(&p.owner, "scan1", "1", "full", true);
    let echoed = call(p.owner.observe_contact_scan(
        &json!({"schema_version": 1, "scan_id": "scan1", "source": written}).to_string(),
    ));
    assert_eq!(echoed["contact_id"], id);
    assert_eq!(echoed["changed"], false);
    assert_eq!(echoed["revision"], "2");

    p.deliver(true);
    let updated = find(&p.peer, &id).unwrap();
    assert_eq!(updated["phones"][0]["id"], field_id);
    assert_eq!(updated["phones"][0]["value"], "+12025550199");
    assert_eq!(requester_state(&p.peer, &p.peer_cfg, "r1"), "applied");
    assert!(!view_raw(&p.peer).contains("native:"));
}

fn try_capture(client: &Client, book: &Value, contacts: Vec<Value>) -> Result<String, Error> {
    client.capture_contact_book(
        &json!({"schema_version": 1, "book": book, "contacts": contacts}).to_string(),
    )
}
fn rich(id: &str, birthday: Value) -> Value {
    let mut c = contact(id, "Ada");
    c["birthday"] = birthday;
    c["addresses"] = json!([{
        "id": "a1", "label": "home", "street": "12 St James's Sq", "city": "London",
        "state": "LDN", "postal_code": "SW1Y 4JH", "country": "United Kingdom", "iso_country_code": "gb"
    }]);
    c
}

#[test]
fn birthdays_and_structured_addresses_capture_edit_and_restore() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    capture(
        &p.owner,
        &b,
        vec![rich("c1", json!({"month": 2, "day": 29}))],
    );
    for bad in [
        json!({"year": 2023, "month": 2, "day": 29}),
        json!({"month": 13, "day": 1}),
        json!({"month": 4, "day": 31}),
        json!({"month": 1}),
        json!({"month": 1, "day": 1, "era": "ad"}),
        json!("1815-12-10"),
    ] {
        assert!(
            try_capture(&p.owner, &b, vec![rich("c2", bad.clone())]).is_err(),
            "{bad}"
        );
    }
    let mut bad_address = rich("c2", Value::Null);
    bad_address["addresses"][0]["planet"] = json!("Mars");
    assert!(try_capture(&p.owner, &b, vec![bad_address]).is_err());
    assert!(
        try_capture(
            &p.owner,
            &b,
            vec![rich("c3", json!({"year": 2024, "month": 2, "day": 29}))]
        )
        .is_ok()
    );
    p.deliver(true);
    let peer_c1 = find(&p.peer, "c1").unwrap();
    assert_eq!(peer_c1["birthday"], json!({"month": 2, "day": 29}));
    assert_eq!(peer_c1["addresses"][0]["postal_code"], "SW1Y 4JH");

    let bad_patch = request(
        &p.owner_cfg,
        "bad",
        "update",
        "1",
        json!({"patches": [{"op": "replace", "path": "birthday", "value": {"month": 2, "day": 30}}]}),
    );
    assert!(p.peer.request_contact_edit(&bad_patch.to_string()).is_err());
    let r = request(
        &p.owner_cfg,
        "r1",
        "update",
        "1",
        json!({"patches": [
            {"op": "replace", "path": "birthday", "value": {"year": 1815, "month": 12, "day": 10}},
            {"op": "replace", "path": "addresses[a1]", "value": {"id": "a1", "label": "work", "city": "Oxford"}}
        ]}),
    );
    call(p.peer.request_contact_edit(&r.to_string()));
    p.deliver(false);
    assert_eq!(permit(&p.owner, "r1")["status"], "permit");
    let mut written = rich("c1", json!({"year": 1815, "month": 12, "day": 10}));
    written["addresses"] = json!([{"id": "a1", "label": "work", "city": "Oxford"}]);
    assert_eq!(
        call(reconcile(&p.owner, "r1", "applied", Some(written)))["status"],
        "applied"
    );
    p.deliver(true);
    let peer_c1 = find(&p.peer, "c1").unwrap();
    assert_eq!(peer_c1["birthday"]["year"], 1815);
    assert_eq!(peer_c1["addresses"][0]["city"], "Oxford");

    // Removal keeps the birthday in the tombstone; restore carries it back.
    let mut d = request(&p.owner_cfg, "d1", "delete", "2", json!({}));
    d["contact_id"] = json!("c1");
    call(p.peer.request_contact_edit(&d.to_string()));
    p.deliver(false);
    // One of two contacts exceeds the rolling 5% delete limit even in Auto.
    assert_eq!(permit(&p.owner, "d1")["status"], "awaiting_approval");
    approve(&p.owner, "request_id", "d1", true);
    assert_eq!(permit(&p.owner, "d1")["status"], "permit");
    call(reconcile(&p.owner, "d1", "applied", None));
    let restore = call(p.owner.restore_contact_json(
        &json!({"schema_version": 1, "book_id": BOOK, "contact_id": "c1"}).to_string(),
    ));
    let list = call(
        p.owner
            .list_contact_requests_json(&json!({"schema_version": 1, "book_id": BOOK}).to_string()),
    );
    let restored = list["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["request_id"] == restore["request_id"])
        .unwrap();
    assert!(
        restored["field_paths"]
            .as_array()
            .unwrap()
            .contains(&json!("birthday"))
    );
}

fn two_account_book(owner: &ClientConfig, mode: &str) -> Value {
    let mut b = book(owner, "1", mode);
    b["accounts"] = json!([
        {"id": "native:acct-secret", "name": "Google", "writable": true},
        {"id": "native:acct-icloud", "name": "iCloud", "writable": true},
        {"id": "native:acct-exchange", "name": "Exchange", "writable": false}
    ]);
    b
}
fn settings(client: &Client, input: Value) -> Result<String, Error> {
    client.contact_settings_json(&input.to_string())
}

#[test]
fn owner_settings_read_and_update_stay_native_only() {
    let mut p = pair();
    capture(
        &p.owner,
        &two_account_book(&p.owner_cfg, "confirm"),
        vec![contact("c1", "Ada")],
    );
    p.deliver(true);
    let read = call(settings(
        &p.owner,
        json!({"schema_version": 1, "book_id": BOOK}),
    ));
    assert_eq!(read["status"], "current");
    assert_eq!(read["default_account_id"], "native:acct-secret");
    assert_eq!(read["accounts"].as_array().unwrap().len(), 3);
    assert_eq!(read["effective_remote_edits"], "confirm");
    assert_eq!(read["policy_source"], "capture");
    assert!(settings(&p.peer, json!({"schema_version": 1, "book_id": BOOK})).is_err());

    for bad in ["native:acct-unknown", "native:acct-exchange"] {
        assert!(
            settings(
                &p.owner,
                json!({"schema_version": 1, "book_id": BOOK, "default_account_id": bad})
            )
            .is_err()
        );
    }
    assert!(
        settings(
            &p.owner,
            json!({"schema_version": 1, "book_id": BOOK, "policy": {"remote_edits": "always"}})
        )
        .is_err()
    );
    let updated = call(settings(
        &p.owner,
        json!({
            "schema_version": 1, "book_id": BOOK,
            "default_account_id": "native:acct-icloud", "policy": {"remote_edits": "auto"}
        }),
    ));
    assert_eq!(updated["status"], "updated");
    assert_eq!(updated["default_account_id"], "native:acct-icloud");
    assert_eq!(updated["effective_remote_edits"], "auto");
    assert_eq!(updated["policy"]["large_delete_requires_approval"], true);

    // A later native capture (stale prefs: confirm + original default) keeps owner settings.
    capture(
        &p.owner,
        &two_account_book(&p.owner_cfg, "confirm"),
        vec![contact("c1", "Ada")],
    );
    let read = call(settings(
        &p.owner,
        json!({"schema_version": 1, "book_id": BOOK}),
    ));
    assert_eq!(read["default_account_id"], "native:acct-icloud");
    assert_eq!(read["effective_remote_edits"], "auto");
    assert_eq!(read["policy_source"], "owner_settings");

    p.deliver(true);
    let books = p.peer.list_contact_books_json().unwrap();
    assert!(!books.contains("native:"), "{books}");
    let books: Value = serde_json::from_str(&books).unwrap();
    assert_eq!(books["books"][0]["effective_remote_edits"], "auto");
    assert!(!view_raw(&p.peer).contains("native:"));
}

#[test]
fn request_list_summarizes_requests_and_surfaces_holds_after_restart() {
    let mut p = pair();
    let all = (0..40)
        .map(|i| contact(&format!("c{i:02}"), "Ada"))
        .collect::<Vec<_>>();
    let mut b = book(&p.owner_cfg, "1", "confirm");
    capture(&p.owner, &b, all.clone());
    p.deliver(true);
    for (id, value) in [("r1", "Augusta"), ("r2", "Ada Augusta")] {
        let mut r = request(&p.owner_cfg, id, "update", "1", given_patch(value));
        r["contact_id"] = json!("c01");
        call(p.peer.request_contact_edit(&r.to_string()));
    }
    p.deliver(false);
    // Approve and permit r2 so it is an `applying` recovery row.
    assert_eq!(permit(&p.owner, "r2")["status"], "awaiting_approval");
    call(p.owner.contact_approval_json(
        &json!({"schema_version": 1, "request_id": "r2", "approve": true}).to_string(),
    ));
    assert_eq!(permit(&p.owner, "r2")["status"], "permit");
    begin(&p.owner, "scan1", "1", "full", true);
    for c in all.iter().take(20) {
        observe(&p.owner, "scan1", c.clone()).unwrap();
    }
    assert_eq!(finish(&p.owner, "scan1", true)["held"], 20);

    // Holds and recovery rows survive a restart.
    let owner_cfg = p.owner_cfg.clone();
    let Pair {
        _dir,
        peer_cfg,
        peer,
        mut server,
        ..
    } = p;
    let vault_owner = &owner_cfg;
    let _ = &mut b;
    let list_raw = {
        let owner = open(vault_owner);
        owner
            .list_contact_requests_json(&json!({"schema_version": 1, "book_id": BOOK}).to_string())
            .unwrap()
    };
    let list: Value = serde_json::from_str(&list_raw).unwrap();
    let requests = list["requests"].as_array().unwrap();
    let r1 = requests.iter().find(|r| r["request_id"] == "r1").unwrap();
    assert_eq!(r1["kind"], "update");
    assert_eq!(r1["contact_id"], "c01");
    assert_eq!(r1["display_name"], "Ada Lovelace");
    assert_eq!(r1["field_paths"], json!(["name.given"]));
    assert_eq!(requests[0]["state"], "applying", "recovery rows sort first");
    let hold = requests
        .iter()
        .find(|r| r["kind"] == "scan_deletions")
        .unwrap();
    assert_eq!(hold["scan_id"], "scan1");
    assert_eq!(hold["count"], 20);
    assert_eq!(hold["sample_names"].as_array().unwrap().len(), 5);
    assert!(!list_raw.contains("Augusta"), "patch values never listed");
    assert!(!list_raw.contains("native:"));
    let bounded: Value = serde_json::from_str(
        &open(vault_owner)
            .list_contact_requests_json(&json!({"book_id": BOOK, "limit": 1}).to_string())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        bounded["requests"].as_array().unwrap().len(),
        2,
        "one request + one hold"
    );
    assert_eq!(bounded["requests"][0]["request_id"], "r2");

    // The requester sees sanitized summaries of its own requests and never owner holds.
    let peer_raw = peer
        .list_contact_requests_json(&json!({"schema_version": 1}).to_string())
        .unwrap();
    let _ = (&peer_cfg, &mut server);
    assert!(!peer_raw.contains("scan_deletions"));
    assert!(!peer_raw.contains("native:"));
    assert!(!peer_raw.contains("Augusta"));
    let peer_list: Value = serde_json::from_str(&peer_raw).unwrap();
    assert!(
        peer_list["requests"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["kind"] == "update")
    );
}

#[test]
fn identical_recaptures_do_not_publish_and_real_changes_do() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    let contacts_v1 = vec![contact("c1", "Ada"), contact("c2", "Bea")];
    capture(&p.owner, &b, contacts_v1.clone());
    assert!(p.server.upload(&p.owner) >= 3);
    for _ in 0..3 {
        capture(&p.owner, &b, contacts_v1.clone());
        assert_eq!(
            p.server.upload(&p.owner),
            0,
            "unchanged rescan must not enqueue"
        );
    }
    assert_eq!(find(&p.owner, "c1").unwrap()["revision"], "1");

    let mut policy = b.clone();
    policy["policy"]["remote_edits"] = json!("off");
    capture(&p.owner, &policy, contacts_v1.clone());
    assert_eq!(
        p.server.upload(&p.owner),
        1,
        "policy change publishes book state"
    );
    let mut paused = policy.clone();
    paused["state"] = json!("paused");
    capture(&p.owner, &paused, contacts_v1.clone());
    assert_eq!(
        p.server.upload(&p.owner),
        1,
        "state change publishes book state"
    );
    capture(
        &p.owner,
        &paused,
        vec![contact("c1", "Augusta"), contact("c2", "Bea")],
    );
    assert_eq!(
        p.server.upload(&p.owner),
        2,
        "contact change publishes upsert and inventory"
    );

    p.server.sync(&p.peer);
    let owner_books: Value =
        serde_json::from_str(&p.owner.list_contact_books_json().unwrap()).unwrap();
    let peer_books: Value =
        serde_json::from_str(&p.peer.list_contact_books_json().unwrap()).unwrap();
    assert_eq!(owner_books["books"][0]["inventory"]["count"], 2);
    assert_eq!(
        peer_books["books"][0]["inventory"],
        owner_books["books"][0]["inventory"]
    );
}

#[test]
fn remote_desktop_restore_reaches_the_owner_permit_without_sources() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    let mut c1 = contact("c1", "Ada");
    c1["birthday"] = json!({"month": 12, "day": 10});
    capture(&p.owner, &b, vec![c1.clone(), contact("c2", "Bea")]);
    begin(&p.owner, "scan1", "1", "full", true);
    observe(&p.owner, "scan1", contact("c2", "Bea")).unwrap();
    assert_eq!(finish(&p.owner, "scan1", true)["deleted"], 1);
    p.deliver(true);
    assert!(find(&p.peer, "c1").is_none());

    // The remote device (not the owner) queues a NEW create for the owner.
    let queued = call(p.peer.restore_contact_json(
        &json!({"schema_version": 1, "book_id": BOOK, "contact_id": "c1"}).to_string(),
    ));
    let request_id = queued["request_id"].as_str().unwrap().to_owned();
    assert!(request_id.starts_with("restore-"));
    assert_eq!(queued["target_owner"], p.owner_cfg.device_id.to_string());
    let peer_list = p
        .peer
        .list_contact_requests_json(
            &json!({"requester": p.peer_cfg.device_id.to_string()}).to_string(),
        )
        .unwrap();
    assert!(peer_list.contains("\"kind\":\"create\"") && peer_list.contains("birthday"));
    assert!(!peer_list.contains("native:"));
    // The requester can never authorize it.
    assert!(
        p.peer
            .next_contact_apply_permit(
                &json!({"schema_version": 1, "request_id": request_id}).to_string()
            )
            .is_err()
    );

    p.deliver(false);
    let granted = permit(&p.owner, &request_id);
    assert_eq!(granted["status"], "permit");
    assert_eq!(granted["request"]["kind"], "create");
    assert_eq!(
        granted["request"]["birthday"],
        json!({"month": 12, "day": 10})
    );
    assert!(!granted["request"].to_string().contains("native:"));
    let new_id = granted["restore"]["contact_id"]
        .as_str()
        .unwrap()
        .to_owned();
    c1["provenance"]["source_id"] = json!("native:src-recreated");
    assert_eq!(
        call(reconcile(&p.owner, &request_id, "applied", Some(c1)))["status"],
        "applied"
    );
    p.deliver(true);
    // A restore never resurrects the old id; it creates a new linked identity.
    assert!(find(&p.peer, "c1").is_none());
    assert_eq!(find(&p.peer, &new_id).unwrap()["birthday"]["month"], 12);
    assert_eq!(
        requester_state(&p.peer, &p.peer_cfg, &request_id),
        "applied"
    );
    // A restored tombstone is no longer restorable.
    assert!(
        p.peer
            .restore_contact_json(&json!({"book_id": BOOK, "contact_id": "c1"}).to_string())
            .is_err()
    );
}

#[test]
fn scan_checkpoint_is_owner_only_bounded_and_survives_reopen() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "auto"),
        vec![contact("c1", "Ada")],
    );
    p.deliver(true);
    let state = |client: &Client, input: Value| client.contact_scan_state_json(&input.to_string());
    assert_eq!(
        call(state(&p.owner, json!({"book_id": BOOK})))["checkpoint"],
        Value::Null
    );
    let checkpoint = json!({"history_token": "opaque-token", "cursor": 5});
    assert_eq!(
        call(state(
            &p.owner,
            json!({"schema_version": 1, "book_id": BOOK, "checkpoint": checkpoint})
        ))["checkpoint"],
        checkpoint
    );
    assert_eq!(p.server.upload(&p.owner), 0, "checkpoints never sync");
    assert!(state(&p.owner, json!({"book_id": BOOK, "checkpoint": "token"})).is_err());
    assert!(
        state(
            &p.owner,
            json!({"book_id": BOOK, "checkpoint": {"t": "x".repeat(70_000)}})
        )
        .is_err()
    );
    assert!(
        state(
            &p.owner,
            json!({"book_id": BOOK, "checkpoint": {}, "clear": true})
        )
        .is_err()
    );
    assert!(
        state(&p.peer, json!({"book_id": BOOK})).is_err(),
        "foreign owner denied"
    );
    assert!(state(&p.peer, json!({"book_id": BOOK, "checkpoint": {"a": 1}})).is_err());

    let reopened = open(&p.owner_cfg);
    assert_eq!(
        call(state(&reopened, json!({"book_id": BOOK})))["checkpoint"],
        checkpoint
    );
    assert_eq!(
        call(state(&reopened, json!({"book_id": BOOK, "clear": true})))["checkpoint"],
        Value::Null
    );
}

#[test]
fn apply_evidence_requires_an_issued_permit_and_is_immutable() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "auto"),
        vec![contact("c1", "Ada")],
    );
    p.deliver(true);
    call(p.peer.request_contact_edit(
        &request(&p.owner_cfg, "r1", "update", "1", given_patch("Augusta")).to_string(),
    ));
    p.deliver(false);
    let evidence =
        |client: &Client, input: Value| client.contact_apply_evidence_json(&input.to_string());
    let ids = json!(["native:b", "native:a", "native:a"]);
    assert!(
        evidence(
            &p.owner,
            json!({"request_id": "r1", "before_source_ids": ids})
        )
        .is_err(),
        "no permit yet"
    );
    assert_eq!(permit(&p.owner, "r1")["status"], "permit");
    let written = call(evidence(
        &p.owner,
        json!({"schema_version": 1, "request_id": "r1", "before_source_ids": ids}),
    ));
    assert_eq!(
        written["evidence"]["before_source_ids"],
        json!(["native:a", "native:b"])
    );
    assert_eq!(
        call(evidence(
            &p.owner,
            json!({"request_id": "r1", "before_source_ids": ["native:a", "native:b"]})
        ))["evidence"],
        written["evidence"]
    );
    assert!(
        evidence(
            &p.owner,
            json!({"request_id": "r1", "before_source_ids": ["native:c"]})
        )
        .is_err(),
        "immutable"
    );
    let too_many = (0..201).map(|i| format!("id{i}")).collect::<Vec<_>>();
    assert!(
        evidence(
            &p.owner,
            json!({"request_id": "r1", "before_source_ids": too_many})
        )
        .is_err()
    );
    assert!(
        evidence(&p.peer, json!({"request_id": "r1"})).is_err(),
        "foreign owner denied"
    );
    assert_eq!(p.server.upload(&p.owner), 0, "evidence never syncs");

    // Recovery sees the same evidence and never a second permit.
    let again = permit(&p.owner, "r1");
    assert_eq!(again["status"], "outcome_unknown");
    assert_eq!(again["evidence"], written["evidence"]);
    call(reconcile(
        &p.owner,
        "r1",
        "applied",
        Some(contact("c1", "Augusta")),
    ));
    assert_eq!(
        call(evidence(&p.owner, json!({"request_id": "r1"})))["evidence"],
        written["evidence"]
    );
}

#[test]
fn books_without_policy_default_to_auto_with_delete_caps() {
    let mut p = pair();
    let mut b = book(&p.owner_cfg, "1", "auto");
    b.as_object_mut().unwrap().remove("policy");
    capture(
        &p.owner,
        &b,
        vec![contact("c1", "Ada"), contact("c2", "Bea")],
    );
    p.deliver(true);
    let read = call(
        p.owner
            .contact_settings_json(&json!({"book_id": BOOK}).to_string()),
    );
    assert_eq!(read["effective_remote_edits"], "auto");
    assert_eq!(read["policy_source"], "default");
    call(p.peer.request_contact_edit(
        &request(&p.owner_cfg, "r1", "update", "1", given_patch("Augusta")).to_string(),
    ));
    let mut d = request(&p.owner_cfg, "d1", "delete", "1", json!({}));
    d["contact_id"] = json!("c2");
    call(p.peer.request_contact_edit(&d.to_string()));
    p.deliver(false);
    assert_eq!(permit(&p.owner, "r1")["status"], "permit");
    assert_eq!(
        permit(&p.owner, "d1")["status"],
        "awaiting_approval",
        "rolling delete cap still applies"
    );
}

/// What the OS reports after recreating a restored contact: a NEW provider source id.
fn recreated(given: &str) -> Value {
    let mut c = contact("c1", given);
    c["provenance"]["source_id"] = json!("native:src-recreated");
    c
}

// ---- Safety regressions (review-core-final H1/M1/M2/M3/L1-L6) ----

/// Uploads the client's pending envelopes in REVERSE producer order (out-of-order upload,
/// e.g. a photo-held upsert released after later rows).
fn upload_reversed(server: &mut Server, client: &Client) {
    let mut pending = client.pending_outbox().unwrap();
    pending.sort_by_key(|e| std::cmp::Reverse(e.producer_sequence.0));
    for envelope in pending {
        client.ack_outbox(envelope.envelope_id).unwrap();
        server.log.push(envelope);
    }
}
fn peer_book(client: &Client) -> Value {
    let books: Value = serde_json::from_str(&client.list_contact_books_json().unwrap()).unwrap();
    books["books"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == BOOK)
        .cloned()
        .unwrap()
}
fn restorable(client: &Client) -> Vec<String> {
    let list = call(client.list_restorable_contacts_json(&json!({"book_id": BOOK}).to_string()));
    list["contacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn delayed_same_generation_upsert_cannot_rewind_retired_or_limited_book() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    capture(&p.owner, &b, vec![contact("c1", "Ada")]);
    p.deliver(true);
    // An upsert embedding the active book is enqueued, then the book is retired.
    capture(
        &p.owner,
        &b,
        vec![contact("c1", "Ada"), contact("c2", "Bea")],
    );
    let mut retired = b.clone();
    retired["state"] = json!("retired");
    capture(&p.owner, &retired, vec![]);
    upload_reversed(&mut p.server, &p.owner);
    p.server.sync(&p.peer);
    assert_eq!(
        peer_book(&p.peer)["state"],
        "retired",
        "late upsert rewound retirement"
    );
    assert!(
        find(&p.peer, "c2").is_some(),
        "the contact itself still applies"
    );

    // Retired is terminal within a generation, even if the host recaptures `active`.
    capture(&p.owner, &b, vec![]);
    assert_eq!(peer_book(&p.owner)["state"], "retired");
    p.deliver(true);
    assert_eq!(peer_book(&p.peer)["state"], "retired");
    // A new generation may reactivate.
    capture(&p.owner, &book(&p.owner_cfg, "2", "auto"), vec![]);
    p.deliver(true);
    assert_eq!(peer_book(&p.peer)["state"], "active");
}

#[test]
fn delayed_upsert_cannot_rewind_limited_capability_or_policy() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    capture(&p.owner, &b, vec![contact("c1", "Ada")]);
    p.deliver(true);
    capture(&p.owner, &b, vec![contact("c1", "Augusta")]);
    let mut limited = b.clone();
    limited["state"] = json!("limited");
    limited["capabilities"]["write"] = json!(false);
    limited["policy"]["remote_edits"] = json!("off");
    capture(&p.owner, &limited, vec![]);
    upload_reversed(&mut p.server, &p.owner);
    p.server.sync(&p.peer);
    let pb = peer_book(&p.peer);
    assert_eq!(pb["state"], "limited");
    assert_eq!(pb["capabilities"]["write"], false);
    assert_eq!(pb["policy"]["remote_edits"], "off");
    assert_eq!(
        find(&p.peer, "c1").unwrap()["display_name"],
        "Augusta Lovelace"
    );
}

#[test]
fn stale_photo_and_partial_expected_old_conflict() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    capture(&p.owner, &b, vec![contact("c1", "Ada")]);
    let mut changed = contact("c1", "Ada");
    changed["nickname"] = json!("Countess");
    capture(&p.owner, &b, vec![changed]); // revision 2
    p.deliver(true);
    let photo = |id: &str, expected: Value| {
        let mut r = request(
            &p.owner_cfg,
            id,
            "update",
            "1",
            json!({"patches": [], "expected_old": expected}),
        );
        r["photo_op"] = json!({"op": "remove"});
        r
    };
    let mut wrong_part = request(
        &p.owner_cfg,
        "n1",
        "update",
        "1",
        json!({"patches": [{"op": "replace", "path": "name.family", "value": "Byron"}]}),
    );
    wrong_part["expected_old"] = json!({"name": {"given": "Ada"}});
    let mut right_part = request(
        &p.owner_cfg,
        "n2",
        "update",
        "1",
        json!({"patches": [{"op": "replace", "path": "name.family", "value": "Byron"}]}),
    );
    right_part["expected_old"] = json!({"name": {"family": "Lovelace"}});
    for r in [
        photo("ph1", json!({})),
        photo("ph2", json!({"photo": null})),
        wrong_part,
        right_part,
    ] {
        call(p.peer.request_contact_edit(&r.to_string()));
    }
    p.deliver(false);
    assert_eq!(
        permit(&p.owner, "ph1")["status"],
        "conflict",
        "stale photo-only update"
    );
    assert_eq!(permit(&p.owner, "ph2")["status"], "permit");
    assert_eq!(
        permit(&p.owner, "n1")["status"],
        "conflict",
        "expected_old must cover the exact path"
    );
    assert_eq!(permit(&p.owner, "n2")["status"], "permit");
}

#[test]
fn restore_mints_one_linked_identity_and_never_duplicates() {
    let mut p = pair();
    let third_cfg = config(&p._dir, "third", &p.vault);
    let third = unlocked(&third_cfg, &p.vault);
    let b = book(&p.owner_cfg, "1", "auto");
    capture(
        &p.owner,
        &b,
        vec![contact("c1", "Ada"), contact("c2", "Bea")],
    );
    begin(&p.owner, "scan1", "1", "full", true);
    observe(&p.owner, "scan1", contact("c2", "Bea")).unwrap();
    finish(&p.owner, "scan1", true);
    p.server.upload(&p.owner);
    p.server.sync(&p.peer);
    p.server.sync(&third);
    // Two requesters restore the same tombstone while offline from each other.
    let a = call(
        p.peer
            .restore_contact_json(&json!({"book_id": BOOK, "contact_id": "c1"}).to_string()),
    );
    let again = call(
        p.peer
            .restore_contact_json(&json!({"book_id": BOOK, "contact_id": "c1"}).to_string()),
    );
    assert_eq!(
        again["request_id"], a["request_id"],
        "requester dedupes its own pending restore"
    );
    let c =
        call(third.restore_contact_json(&json!({"book_id": BOOK, "contact_id": "c1"}).to_string()));
    let (a, c) = (
        a["request_id"].as_str().unwrap().to_owned(),
        c["request_id"].as_str().unwrap().to_owned(),
    );
    p.server.upload(&p.peer);
    p.server.upload(&third);
    p.server.sync(&p.owner);

    let granted = permit(&p.owner, &a);
    assert_eq!(granted["status"], "permit");
    let new_id = granted["restore"]["contact_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(new_id, "c1");
    assert_eq!(granted["restore"]["restored_from"], "c1");
    let second = permit(&p.owner, &c);
    assert_eq!(second["status"], "rejected");
    assert_eq!(second["reason"], "already_restored");
    // Legacy observed DTO still names the old id; core assigns the claimed new identity.
    assert_eq!(
        call(reconcile(&p.owner, &a, "applied", Some(recreated("Ada"))))["status"],
        "applied"
    );
    assert!(find(&p.owner, "c1").is_none(), "old id stays deleted");
    assert_eq!(
        find(&p.owner, &new_id).unwrap()["display_name"],
        "Ada Lovelace"
    );
    assert!(
        restorable(&p.owner).is_empty(),
        "restored tombstone is hidden"
    );

    p.server.upload(&p.owner);
    p.server.sync(&p.peer);
    assert!(find(&p.peer, "c1").is_none());
    assert!(find(&p.peer, &new_id).is_some());
    assert!(
        restorable(&p.peer).is_empty(),
        "peers learn the link from the owner"
    );
    assert!(
        p.peer
            .restore_contact_json(&json!({"book_id": BOOK, "contact_id": "c1"}).to_string())
            .is_err()
    );
    assert_eq!(requester_state(&p.peer, &p.peer_cfg, &a), "applied");
}

#[test]
fn failed_restore_releases_the_claim_but_unknown_keeps_it() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    capture(
        &p.owner,
        &b,
        vec![contact("c1", "Ada"), contact("c2", "Bea")],
    );
    begin(&p.owner, "scan1", "1", "full", true);
    observe(&p.owner, "scan1", contact("c2", "Bea")).unwrap();
    finish(&p.owner, "scan1", true);
    p.deliver(true);
    let r1 = call(
        p.peer
            .restore_contact_json(&json!({"book_id": BOOK, "contact_id": "c1"}).to_string()),
    )["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    p.deliver(false);
    assert_eq!(permit(&p.owner, &r1)["status"], "permit");
    assert_eq!(permit(&p.owner, &r1)["status"], "outcome_unknown");
    // A unknown outcome keeps the claim: an owner-local retry is refused.
    let local = call(
        p.owner
            .restore_contact_json(&json!({"book_id": BOOK, "contact_id": "c1"}).to_string()),
    )["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(permit(&p.owner, &local)["reason"], "already_restored");
    assert_eq!(
        call(reconcile(&p.owner, &r1, "failed", None))["status"],
        "failed"
    );
    p.deliver(true);
    let r2 = call(
        p.peer
            .restore_contact_json(&json!({"book_id": BOOK, "contact_id": "c1"}).to_string()),
    )["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(r2, r1);
    p.deliver(false);
    assert_eq!(
        permit(&p.owner, &r2)["status"],
        "permit",
        "definitive failure may retry"
    );
}

#[test]
fn later_complete_scan_invalidates_earlier_hold() {
    let p = pair();
    let all = (0..40)
        .map(|i| contact(&format!("c{i:02}"), "Ada"))
        .collect::<Vec<_>>();
    capture(&p.owner, &book(&p.owner_cfg, "1", "auto"), all.clone());
    begin(&p.owner, "glitch", "1", "full", true);
    for c in all.iter().take(20) {
        observe(&p.owner, "glitch", c.clone()).unwrap();
    }
    assert_eq!(finish(&p.owner, "glitch", true)["held"], 20);
    begin(&p.owner, "full", "1", "full", true);
    for c in &all {
        observe(&p.owner, "full", c.clone()).unwrap();
    }
    assert_eq!(finish(&p.owner, "full", true)["deleted"], 0);
    let decided = approve(&p.owner, "scan_id", "glitch", true);
    assert_eq!(decided["status"], "unchanged", "{decided}");
    assert_eq!(decided["state"], "stale");
    assert_eq!(contacts(&p.owner).len(), 40);
}

#[test]
fn contacts_written_after_scan_begin_are_not_inferred_missing() {
    let p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    capture(&p.owner, &b, vec![contact("c1", "Ada")]);
    begin(&p.owner, "scan1", "1", "full", true);
    capture(&p.owner, &b, vec![contact("late", "Lee")]);
    observe(&p.owner, "scan1", contact("c1", "Ada")).unwrap();
    assert_eq!(finish(&p.owner, "scan1", true)["deleted"], 0);
    assert!(find(&p.owner, "late").is_some());
}

#[test]
fn fresh_device_keeps_tombstone_that_precedes_book_state() {
    let mut p = pair();
    let b = book(&p.owner_cfg, "1", "auto");
    capture(&p.owner, &b, vec![contact("c1", "Ada")]);
    p.owner
        .pending_outbox()
        .unwrap()
        .iter()
        .for_each(|e| p.owner.ack_outbox(e.envelope_id).unwrap());
    begin(&p.owner, "scan1", "1", "full", true);
    assert_eq!(finish(&p.owner, "scan1", true)["deleted"], 1);
    // Compacted order: only the removal then the latest book state survive.
    p.server.upload(&p.owner);
    let report = p.server.sync(&p.peer);
    assert_eq!(report.quarantined, 0);
    assert_eq!(restorable(&p.peer), vec!["c1".to_owned()]);
}

#[test]
fn read_only_default_account_is_not_preserved() {
    let p = pair();
    capture(&p.owner, &two_account_book(&p.owner_cfg, "auto"), vec![]);
    call(settings(
        &p.owner,
        json!({"book_id": BOOK, "default_account_id": "native:acct-icloud"}),
    ));
    let mut b = two_account_book(&p.owner_cfg, "auto");
    b["accounts"][1]["writable"] = json!(false);
    capture(&p.owner, &b, vec![]);
    let read = call(settings(&p.owner, json!({"book_id": BOOK})));
    assert_eq!(read["default_account_id"], "native:acct-secret");
}

#[test]
fn reopened_permit_publishes_outcome_unknown_once() {
    let mut p = pair();
    capture(
        &p.owner,
        &book(&p.owner_cfg, "1", "auto"),
        vec![contact("c1", "Ada")],
    );
    p.deliver(true);
    call(p.peer.request_contact_edit(
        &request(&p.owner_cfg, "r1", "update", "1", given_patch("Augusta")).to_string(),
    ));
    p.deliver(false);
    assert_eq!(permit(&p.owner, "r1")["status"], "permit");
    assert_eq!(permit(&p.owner, "r1")["status"], "outcome_unknown");
    assert_eq!(p.deliver(true).applied, 1);
    assert_eq!(
        requester_state(&p.peer, &p.peer_cfg, "r1"),
        "outcome_unknown"
    );
    assert_eq!(permit(&p.owner, "r1")["status"], "outcome_unknown");
    assert_eq!(p.server.upload(&p.owner), 0, "published once");
}

#[test]
fn large_delete_approval_cannot_be_disabled() {
    let p = pair();
    let mut b = book(&p.owner_cfg, "1", "auto");
    b["policy"]["large_delete_requires_approval"] = json!(false);
    capture(&p.owner, &b, vec![]);
    let read = call(settings(&p.owner, json!({"book_id": BOOK})));
    assert_eq!(read["policy"]["large_delete_requires_approval"], true);
    assert!(
        settings(
            &p.owner,
            json!({"book_id": BOOK, "policy": {"large_delete_requires_approval": false}})
        )
        .is_err()
    );
    assert!(
        settings(
            &p.owner,
            json!({"book_id": BOOK, "policy": {"large_delete_requires_approval": true}})
        )
        .is_ok()
    );
}
