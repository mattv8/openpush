//! Old encrypted contact edit events stay readable after key rotation, but may not
//! authorize a new OS contact write under a retired epoch.

mod common;
use common::*;
use openpush_crypto::{create_vault_check_header, derive_root_key};
use serde_json::{Value, json};
use tempfile::TempDir;

const BOOK: &str = "book-epoch";
const NEXT_PASSPHRASE: &str = "epoch two has a distinct passphrase";

fn next_epoch(id: VaultId) -> Vault {
    let profile = KeyProfile::new(id.0, 2).unwrap();
    let root = derive_root_key(NEXT_PASSPHRASE, &profile).unwrap();
    let header = create_vault_check_header(&root, profile.clone()).unwrap();
    Vault {
        id,
        profile,
        header,
    }
}

fn call(result: Result<String, Error>) -> Value {
    serde_json::from_str(&result.unwrap()).unwrap()
}

fn book(owner: &ClientConfig) -> Value {
    json!({
        "id": BOOK,
        "owner_device_id": owner.device_id.to_string(),
        "generation": "1",
        "state": "active",
        "capabilities": {"read": true, "write": true, "photo": true},
        "accounts": [],
        "policy": {"remote_edits": "auto", "large_delete_requires_approval": true}
    })
}

fn contact() -> Value {
    json!({
        "id": "contact-1",
        "book_id": BOOK,
        "revision": "1",
        "display_name": "Ada Lovelace",
        "name": {"given": "Ada", "family": "Lovelace"},
        "phones": [],
        "emails": [],
        "addresses": []
    })
}

fn request(owner: &ClientConfig, request_id: &str) -> Value {
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 86_400;
    json!({
        "schema_version": 1,
        "request_id": request_id,
        "target_owner": owner.device_id.to_string(),
        "book_id": BOOK,
        "kind": "update",
        "contact_id": "contact-1",
        "base_revision": "1",
        "patches": [{"op": "replace", "path": "name.given", "value": "Augusta"}],
        "expires_at": expires_at
    })
}

fn permit(owner: &Client, request_id: &str) -> Value {
    call(owner.next_contact_apply_permit(
        &json!({"schema_version": 1, "request_id": request_id}).to_string(),
    ))
}

struct Pair {
    _dir: TempDir,
    epoch1: Vault,
    epoch2: Vault,
    owner_cfg: ClientConfig,
    owner: Client,
    peer: Client,
    server: Server,
}

fn pair() -> Pair {
    let dir = TempDir::new().unwrap();
    let epoch1 = Vault::new();
    let epoch2 = next_epoch(epoch1.id);
    let owner_cfg = config(&dir, "owner", &epoch1);
    let peer_cfg = config(&dir, "peer", &epoch1);
    let owner = unlocked(&owner_cfg, &epoch1);
    let peer = unlocked(&peer_cfg, &epoch1);
    let mut server = Server::default();

    call(
        owner.capture_contact_book(
            &json!({"schema_version": 1, "book": book(&owner_cfg), "contacts": [contact()]})
                .to_string(),
        ),
    );
    server.upload(&owner);
    assert_eq!(server.sync(&peer).quarantined, 0);

    Pair {
        _dir: dir,
        epoch1,
        epoch2,
        owner_cfg,
        owner,
        peer,
        server,
    }
}

fn rotate_owner(pair: &Pair) {
    pair.owner
        .unlock(&pair.epoch2.profile, &pair.epoch2.header, NEXT_PASSPHRASE)
        .unwrap();
    pair.owner.activate_epoch(2).unwrap();
}

#[test]
fn current_epoch_request_after_cutover_can_receive_a_permit() {
    let mut pair = pair();
    rotate_owner(&pair);
    pair.peer
        .unlock(&pair.epoch2.profile, &pair.epoch2.header, NEXT_PASSPHRASE)
        .unwrap();
    pair.peer.activate_epoch(2).unwrap();
    call(
        pair.peer
            .request_contact_edit(&request(&pair.owner_cfg, "current-epoch").to_string()),
    );
    pair.server.upload(&pair.peer);
    assert_eq!(pair.server.sync(&pair.owner).quarantined, 0);
    assert_eq!(permit(&pair.owner, "current-epoch")["status"], "permit");
}

#[test]
fn legacy_ledger_migration_preserves_data_without_inventing_request_authority() {
    let mut pair = pair();
    call(
        pair.peer
            .request_contact_edit(&request(&pair.owner_cfg, "legacy-epoch").to_string()),
    );
    pair.server.upload(&pair.peer);
    assert_eq!(pair.server.sync(&pair.owner).quarantined, 0);
    drop(pair.owner);

    // Simulate the prior local schema in this isolated encrypted test database.
    let conn = rusqlite::Connection::open(&pair.owner_cfg.database_path).unwrap();
    let raw_key = DB_KEY
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    conn.pragma_update(None, "key", format!("x'{raw_key}'"))
        .unwrap();
    conn.execute("ALTER TABLE contact_edit_ledger DROP COLUMN key_epoch", [])
        .unwrap();
    drop(conn);

    pair.owner = unlocked(&pair.owner_cfg, &pair.epoch1);
    let decision = permit(&pair.owner, "legacy-epoch");
    assert_eq!(decision["status"], "rejected");
    assert_eq!(decision["reason"], "retired_epoch");
    let view = call(
        pair.owner
            .contact_book_view(&json!({"book_id": BOOK}).to_string()),
    );
    assert_eq!(view["contacts"][0]["display_name"], "Ada Lovelace");
}

#[test]
fn old_epoch_request_arriving_after_cutover_cannot_receive_new_permit() {
    let mut pair = pair();
    call(
        pair.peer
            .request_contact_edit(&request(&pair.owner_cfg, "after-cutover").to_string()),
    );
    assert_eq!(pair.peer.pending_outbox().unwrap()[0].key_epoch, 1);

    rotate_owner(&pair);
    pair.server.upload(&pair.peer);
    assert_eq!(pair.server.sync(&pair.owner).quarantined, 0);

    let decision = permit(&pair.owner, "after-cutover");
    assert_eq!(decision["status"], "rejected");
    assert_eq!(decision["reason"], "retired_epoch");
    let view: Value = serde_json::from_str(
        &pair
            .owner
            .contact_book_view(
                &json!({"schema_version": 1, "book_id": BOOK, "limit": 10}).to_string(),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(view["contacts"][0]["display_name"], "Ada Lovelace");
}

#[test]
fn old_epoch_request_queued_before_cutover_cannot_receive_new_permit() {
    let mut pair = pair();
    call(
        pair.peer
            .request_contact_edit(&request(&pair.owner_cfg, "queued-before-cutover").to_string()),
    );
    pair.server.upload(&pair.peer);
    assert_eq!(pair.server.sync(&pair.owner).quarantined, 0);

    rotate_owner(&pair);
    let decision = permit(&pair.owner, "queued-before-cutover");
    assert_eq!(decision["status"], "rejected");
    assert_eq!(decision["reason"], "retired_epoch");
}

#[test]
fn issued_old_epoch_permit_stays_reconcilable_after_cutover() {
    let mut pair = pair();
    call(
        pair.peer
            .request_contact_edit(&request(&pair.owner_cfg, "issued-before-cutover").to_string()),
    );
    pair.server.upload(&pair.peer);
    assert_eq!(pair.server.sync(&pair.owner).quarantined, 0);
    assert_eq!(
        permit(&pair.owner, "issued-before-cutover")["status"],
        "permit"
    );

    rotate_owner(&pair);
    let reopened = permit(&pair.owner, "issued-before-cutover");
    assert_eq!(reopened["status"], "outcome_unknown");
    let reconciled = call(
        pair.owner.reconcile_contact_apply(
            &json!({
                "schema_version": 1,
                "request_id": "issued-before-cutover",
                "outcome": "unknown"
            })
            .to_string(),
        ),
    );
    assert_eq!(reconciled["status"], "outcome_unknown");
    assert_eq!(
        permit(&pair.owner, "issued-before-cutover")["status"],
        "outcome_unknown"
    );
}
