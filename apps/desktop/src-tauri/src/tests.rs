//! Host integration tests over real client-core databases (SQLCipher) with an in-memory secure
//! store. `real_server_*` tests additionally run the actual server router over TCP against the
//! local PostgreSQL (and SeaweedFS for media); they are ignored unless explicitly requested.
use crate::{
    credentials::{
        ensure_database_key, parse_credential, store_credential, tests::credential_json,
        tests::TOKEN, Binding,
    },
    dto::GatewayView,
    media::tests::sample_png,
    secure_store::{MemoryStore, SecretStore},
    session::{open_session, DraftInput, Notifier, Session},
};
use openpush_client_core::{KeyProfile, VaultCheckHeader};
use openpush_crypto::{create_vault_check_header, derive_root_key};
use std::{path::Path, sync::Arc};

pub const PHRASE: &str = "correct horse battery staple";

pub(crate) fn noop() -> Notifier {
    Arc::new(|| {})
}

pub(crate) struct Fixture {
    pub dir: tempfile::TempDir,
    pub store: MemoryStore,
    pub binding: Binding,
    pub profile: KeyProfile,
    pub header: VaultCheckHeader,
}

fn fixture() -> Fixture {
    // Port 9 (discard) on loopback: nothing listens; these tests never touch the network.
    fixture_at("http://127.0.0.1:9")
}

pub(crate) fn fixture_at(origin: &str) -> Fixture {
    let vault = uuid::Uuid::new_v4();
    let device = uuid::Uuid::new_v4();
    let profile = KeyProfile::new(vault, 1).unwrap();
    let root = derive_root_key(PHRASE, &profile).unwrap();
    let header = create_vault_check_header(&root, profile.clone()).unwrap();
    let store = MemoryStore::default();
    let credential = parse_credential(&credential_json(
        origin,
        &vault.to_string(),
        &device.to_string(),
        TOKEN,
    ))
    .unwrap();
    store_credential(&store, &credential).unwrap();
    Fixture {
        dir: tempfile::tempdir().unwrap(),
        store,
        binding: credential.binding(),
        profile,
        header,
    }
}

pub(crate) fn open(f: &Fixture, binding: &Binding, epochs: &[u32]) -> Session {
    open_session(f.dir.path(), &f.store, binding, epochs, noop()).unwrap()
}

pub(crate) fn input(
    id: &str,
    conversation: &str,
    text: &str,
    recipients: &[&str],
    revision: &str,
) -> DraftInput {
    serde_json::from_value(serde_json::json!({
        "id": id, "conversationId": conversation, "text": text, "recipientIds": recipients,
        "attachmentIds": [], "expectedRevision": revision, "revision": "ignored-extra-field"
    }))
    .unwrap()
}

pub(crate) fn routed(mut draft: DraftInput, gateway: &str, sim: &str) -> DraftInput {
    draft.gateway_id = Some(gateway.into());
    draft.sim_id = Some(sim.into());
    draft
}

pub(crate) fn gateway(id: &str, sms: bool, mms: bool) -> GatewayView {
    GatewayView {
        id: id.into(),
        name: "Test SIM".into(),
        sim_id: "sim-1".into(),
        online: false,
        simulated: true,
        supports_sms: sms,
        supports_mms: mms,
        capability_note: None,
    }
}

fn assert_sanitized(json: &str, dir: &Path) {
    for forbidden in [
        TOKEN,
        "file_key",
        "fileKey",
        "deviceToken",
        "db-key",
        "sqlcipher",
        ".opss",
    ] {
        assert!(!json.contains(forbidden), "snapshot leaked {forbidden}");
    }
    assert!(
        !json.contains(&dir.to_string_lossy().to_string()),
        "snapshot leaked a local path"
    );
}

#[test]
fn drafts_send_atomically_through_checked_routes_and_map_to_sanitized_dtos() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    session
        .client
        .unlock(&f.profile, &f.header, PHRASE)
        .unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();

    // New-conversation draft from the UI placeholder; recipients normalized; extra fields ignored.
    let draft = session
        .save_draft(&input(
            "draft-new",
            "",
            "hello",
            &["+1 (555) 555-0100"],
            "0",
        ))
        .unwrap();
    assert_eq!(draft.revision, "1");
    assert_eq!(draft.recipient_ids, vec!["+15555550100".to_string()]);
    assert!(session
        .save_draft(&input(
            &draft.id,
            &draft.conversation_id,
            "x",
            &["not a phone"],
            "1"
        ))
        .is_err());
    assert_eq!(
        session
            .save_draft(&input(&draft.id, &draft.conversation_id, "stale", &[], "0"))
            .unwrap_err()
            .code,
        "stale-draft"
    );

    // Gateway capabilities not loaded yet: refused, draft untouched.
    let send = routed(
        input(&draft.id, &draft.conversation_id, "", &[], "1"),
        &gateway_id,
        "sim-1",
    );
    assert_eq!(
        session.send_draft(&send).unwrap_err().code,
        "gateways-unknown"
    );
    session.set_status(|s| {
        s.gateways = vec![gateway(&gateway_id, true, false)];
        s.gateways_known = true;
    });
    // Unreported route and stale revision are refused without queueing anything.
    assert_eq!(
        session
            .send_draft(&routed(
                input(&draft.id, "", "", &[], "1"),
                &gateway_id,
                "sim-9"
            ))
            .unwrap_err()
            .code,
        "gateway-unavailable"
    );
    assert_eq!(
        session
            .send_draft(&routed(
                input(&draft.id, "", "", &[], "0"),
                &gateway_id,
                "sim-1"
            ))
            .unwrap_err()
            .code,
        "stale-draft"
    );
    assert!(session.client.pending_outbox_batch(10).unwrap().is_empty());

    // Accepted == durably queued locally; the route was persisted with CAS before the send.
    let result = session.send_draft(&send).unwrap();
    assert!(result.accepted);
    assert_eq!(result.status, "queued-local");
    assert_eq!(session.client.pending_outbox_batch(10).unwrap().len(), 1);
    // Double submit of the same revision cannot queue a second command.
    assert_eq!(session.send_draft(&send).unwrap_err().code, "stale-draft");
    assert_eq!(session.client.pending_outbox_batch(10).unwrap().len(), 1);

    let (snapshot, _) = session
        .snapshot(
            Some(&draft.conversation_id),
            crate::head(),
            Some("http://127.0.0.1:9".into()),
        )
        .unwrap();
    let conversation = snapshot
        .conversations
        .iter()
        .find(|c| c.id == draft.conversation_id)
        .unwrap();
    assert_eq!(conversation.name, "+15555550100");
    let message = conversation.messages.last().unwrap();
    assert_eq!(
        (message.body.as_str(), message.sender, message.status),
        ("hello", "self", Some("queued-local"))
    );
    assert!(
        message.timestamp.is_empty(),
        "no fabricated wall-clock time"
    );
    let stored = snapshot.draft.as_ref().unwrap();
    assert_eq!(stored.text, "");
    assert_eq!(stored.gateway_id.as_deref(), Some(gateway_id.as_str()));
    assert_eq!(stored.sim_id.as_deref(), Some("sim-1"));
    assert_eq!(snapshot.encryption.state, "unlocked");
    assert_eq!(snapshot.pending_count, 1);
    assert_sanitized(&serde_json::to_string(&snapshot).unwrap(), f.dir.path());

    // MMS: attachments need an MMS-capable route; previews are native re-encoded data URLs.
    let png = f.dir.path().join("photo.png");
    std::fs::write(&png, sample_png(64, 48)).unwrap();
    let attachment = session.prepare_attachment(&png).unwrap();
    assert_eq!(attachment.media_type, "image/png");
    assert!(attachment
        .preview_url
        .as_deref()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    let mut with_media = input(
        &stored.id,
        &stored.conversation_id,
        "",
        &[],
        &stored.revision,
    );
    with_media.attachment_ids = vec![attachment.id.clone()];
    let saved = session.save_draft(&with_media).unwrap();
    assert_eq!(
        saved.gateway_id.as_deref(),
        Some(gateway_id.as_str()),
        "route survives saves that omit it"
    );
    let send_mms = routed(
        input(&saved.id, "", "", &[], &saved.revision),
        &gateway_id,
        "sim-1",
    );
    assert_eq!(
        session.send_draft(&send_mms).unwrap_err().code,
        "mms-unsupported"
    );
    session.set_status(|s| s.gateways = vec![gateway(&gateway_id, true, true)]);
    assert!(session.send_draft(&send_mms).unwrap().accepted);
    assert_eq!(
        session.client.pending_uploads().unwrap().len(),
        1,
        "media upload is pending before the MMS can seal"
    );
    let (snapshot, _) = session
        .snapshot(Some(&draft.conversation_id), crate::head(), None)
        .unwrap();
    let message = snapshot
        .conversations
        .iter()
        .find(|c| c.id == draft.conversation_id)
        .unwrap()
        .messages
        .last()
        .unwrap()
        .attachments
        .clone();
    assert_eq!(message.len(), 1);
    assert_eq!(message[0].state, "uploading");
    assert!(message[0]
        .preview_url
        .as_deref()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    assert_sanitized(&serde_json::to_string(&snapshot).unwrap(), f.dir.path());

    assert_eq!(
        session.mark_seen(&["nope".into()]).unwrap_err().code,
        "invalid-message"
    );
}

#[test]
fn reopen_preserves_key_and_cached_unlock_and_bindings_never_share_clients() {
    let f = fixture();
    let first_key = {
        let session = open(&f, &f.binding, &[]);
        session
            .client
            .unlock(&f.profile, &f.header, PHRASE)
            .unwrap();
        session
            .save_draft(&input("draft-new", "", "kept", &["+15555550100"], "0"))
            .unwrap();
        session.persist_gateways(&[gateway(&uuid::Uuid::new_v4().to_string(), true, false)]);
        let cache = session.client.export_native_key_cache(1).unwrap();
        f.store
            .set(
                &f.binding.key_cache_account(1),
                cache.native_storage_bytes(),
            )
            .unwrap();
        f.store
            .get(&f.binding.db_key_account())
            .unwrap()
            .unwrap()
            .to_vec()
    };
    // Reimport/reopen: the existing key is preserved and the database opens with its data;
    // the native key cache restores the unlocked state without a passphrase.
    assert_eq!(
        ensure_database_key(&f.store, &f.binding, &f.binding.database_path(f.dir.path()))
            .unwrap()
            .as_slice(),
        first_key.as_slice()
    );
    let reopened = open(&f, &f.binding, &[1]);
    assert_eq!(reopened.client.compose_drafts().unwrap()[0].text, "kept");
    let keys = reopened.client.key_status().unwrap();
    assert!(keys.active_epoch == Some(1) && keys.unlocked_epochs.contains(&1));
    let (gateways, known) = reopened.gateways();
    assert!(
        known && gateways.len() == 1,
        "last reported routes survive restart for offline sends"
    );
    drop(reopened);

    // Another device of the same vault: separate key, database and client; no shared state.
    let other_device = uuid::Uuid::new_v4().to_string();
    let other = parse_credential(&credential_json(
        "http://127.0.0.1:9",
        &f.binding.vault_id,
        &other_device,
        TOKEN,
    ))
    .unwrap();
    store_credential(&f.store, &other).unwrap();
    let second = open(&f, &other.binding(), &[1]);
    assert!(second.client.compose_drafts().unwrap().is_empty());
    assert_ne!(
        f.store
            .get(&other.binding().db_key_account())
            .unwrap()
            .unwrap()
            .to_vec(),
        first_key
    );
    assert_eq!(
        second.client.key_status().unwrap().active_epoch,
        None,
        "another device's cache is never applied"
    );
    drop(second);

    // A wrong stored key is reported, never treated as a new database; nothing is reset.
    f.store
        .set(&f.binding.db_key_account(), &[7u8; 32])
        .unwrap();
    let error = open_session(f.dir.path(), &f.store, &f.binding, &[], noop())
        .err()
        .unwrap();
    assert_eq!(error.code, "database-key-mismatch");
    f.store
        .set(&f.binding.db_key_account(), &first_key)
        .unwrap();
    assert_eq!(
        open(&f, &f.binding, &[]).client.compose_drafts().unwrap()[0].text,
        "kept"
    );
}

#[test]
fn missing_credential_or_origin_mismatch_fails_closed() {
    let f = fixture();
    let mut moved = f.binding.clone();
    moved.origin = "https://elsewhere.test".into();
    assert_eq!(
        open_session(f.dir.path(), &f.store, &moved, &[], noop())
            .err()
            .unwrap()
            .code,
        "origin-binding"
    );
    let mut unknown = f.binding.clone();
    unknown.device_id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        open_session(f.dir.path(), &f.store, &unknown, &[], noop())
            .err()
            .unwrap()
            .code,
        "credentials-required"
    );
}

/// Real server router + PostgreSQL over TCP, real HTTP and WebSocket from the native supervisor.
#[cfg(test)]
mod real_server {
    use super::*;
    use crate::{session::VaultSummary, sync, AppState};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use ed25519_dalek::{Signer, SigningKey};
    use openpush_client_core::{
        Client, ClientConfig, Cursor, DatabaseKey, DeviceId, IncomingSms, PermitDecision,
        SendResult, VaultId,
    };
    use serde_json::{json, Value};
    use sqlx::postgres::PgPoolOptions;
    use std::{str::FromStr, time::Duration};

    const ADDRESS: &str = "+15555550123";

    struct Server {
        url: String,
        owner: String,
        vault: VaultId,
        profile: KeyProfile,
        header: VaultCheckHeader,
        fingerprint: String,
        admin: sqlx::PgPool,
        pool: sqlx::PgPool,
        schema: String,
        task: tokio::task::JoinHandle<()>,
    }

    async fn start() -> Server {
        let database_url = std::env::var("TEST_DATABASE_URL").expect(
            "TEST_DATABASE_URL required; source .opencode/sessions/messaging-foundation/test.env",
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .unwrap();
        let schema = format!("openpush_desktop_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut isolated: url::Url = database_url.parse().unwrap();
        isolated
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(isolated.as_str())
            .await
            .unwrap();
        sqlx::migrate!("../../../services/server/migrations")
            .run(&pool)
            .await
            .unwrap();
        let vault = VaultId::new();
        let profile = KeyProfile::new(vault.0, 1).unwrap();
        let fingerprint = profile.fingerprint().unwrap();
        let root = derive_root_key(PHRASE, &profile).unwrap();
        let header = create_vault_check_header(&root, profile.clone()).unwrap();
        let owner = openpush_server::api::create_owner(
            &pool,
            serde_json::to_value(&profile).unwrap(),
            serde_json::to_vec(&header).unwrap(),
            fingerprint.clone(),
            1,
        )
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_pool = pool.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, openpush_server::api::router(server_pool))
                .await
                .unwrap();
        });
        Server {
            url: format!("http://{address}"),
            owner: owner.device_token,
            vault,
            profile,
            header,
            fingerprint,
            admin,
            pool,
            schema,
            task,
        }
    }

    impl Server {
        async fn pair(&self, role: &str) -> (DeviceId, String) {
            let key = SigningKey::from_bytes(&[11; 32]);
            let id = DeviceId::new();
            let public = json!({"ed25519_public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())});
            let http = reqwest::Client::new();
            let challenge: Value = http.post(format!("{}/v1/pairing", self.url)).bearer_auth(&self.owner)
                .json(&json!({"device_id":id.0,"public_key":public,"profile_fingerprint":self.fingerprint,"key_epoch":1,"requested_role":role}))
                .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
            let token = challenge["challenge_token"].as_str().unwrap();
            let raw: [u8; 32] = URL_SAFE_NO_PAD.decode(token).unwrap().try_into().unwrap();
            let proof = openpush_protocol::pairing_proof_message(
                &raw,
                self.vault,
                id,
                &self.fingerprint,
                1,
                role,
            );
            let body: Value = http.post(format!("{}/v1/pairing/consume", self.url))
                .json(&json!({"challenge_token":token,"device_id":id.0,"public_key":public,"profile_fingerprint":self.fingerprint,"key_epoch":1,"signature":URL_SAFE_NO_PAD.encode(key.sign(&proof).to_bytes())}))
                .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
            (id, body["device_token"].as_str().unwrap().to_owned())
        }
        async fn shutdown(self) {
            self.task.abort();
            let _ = self.task.await;
            sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
                .execute(&self.admin)
                .await
                .unwrap();
        }
    }

    /// Minimal gateway peer using only public client-core APIs and the server's HTTP routes.
    struct Gateway {
        client: Client,
        token: String,
        url: String,
        _dir: tempfile::TempDir,
    }
    impl Gateway {
        async fn sync(&self) {
            let http = reqwest::Client::new();
            for envelope in self.client.pending_outbox().unwrap() {
                http.post(format!("{}/v1/events", self.url))
                    .bearer_auth(&self.token)
                    .json(&envelope)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
                self.client.ack_outbox(envelope.envelope_id).unwrap();
            }
            let after = self.client.receive_cursor().unwrap().0;
            let page: Value = http
                .get(format!("{}/v1/events?after={after}&limit=100", self.url))
                .bearer_auth(&self.token)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            for event in page["events"].as_array().unwrap() {
                let cursor: u64 = event["cursor"].as_str().unwrap().parse().unwrap();
                self.client
                    .ingest_raw(event["envelope"].to_string().as_bytes(), Cursor(cursor))
                    .unwrap();
            }
            while self.client.apply_pending(1000).unwrap().applied > 0 {}
        }
    }

    async fn until(what: &str, seconds: u64, mut check: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        while !check() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires TEST_DATABASE_URL (local PostgreSQL); run with --include-ignored, see reports/DU-final.md"]
    async fn real_server_import_unlock_ws_send_status_and_incoming() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let server = start().await;
        let (desktop_id, desktop_token) = server.pair("device").await;
        let (gateway_id, gateway_token) = server.pair("gateway").await;
        // The gateway reports one simulated SIM through the capability contract.
        reqwest::Client::new().post(format!("{}/v1/capabilities", server.url)).bearer_auth(&gateway_token)
            .json(&json!({"simulator":true,"capabilities":{"sims":[{"subscription_id":"sim-1","label":"SIM 1","sms":"available","mms":"unsupported"}]}}))
            .send().await.unwrap().error_for_status().unwrap();
        let gateway_dir = tempfile::tempdir().unwrap();
        let gateway = Gateway {
            client: Client::open(
                ClientConfig {
                    database_path: gateway_dir.path().join("gateway.db"),
                    vault_id: server.vault,
                    device_id: gateway_id,
                },
                DatabaseKey::new(&[3; 32]).unwrap(),
            )
            .unwrap(),
            token: gateway_token,
            url: server.url.clone(),
            _dir: gateway_dir,
        };
        gateway
            .client
            .unlock(&server.profile, &server.header, PHRASE)
            .unwrap();

        // Desktop: import through the same path as the native command (secure store = memory).
        let root = tempfile::tempdir().unwrap();
        let hints = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hints.clone();
        let state = AppState::new(
            root.path().to_path_buf(),
            Arc::new(MemoryStore::default()),
            Arc::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }),
        );
        // A credential for another origin is refused once a server is configured.
        state
            .update_config(|config| config.select_origin(&server.url))
            .unwrap();
        let foreign = parse_credential(&credential_json(
            "https://elsewhere.test",
            &server.vault.to_string(),
            &desktop_id.to_string(),
            &desktop_token,
        ))
        .unwrap();
        assert_eq!(
            crate::import_credential(&state, Arc::new(foreign))
                .await
                .unwrap_err()
                .code,
            "origin-binding"
        );
        // A wrong token is rejected by the real server before anything is stored.
        let wrong = parse_credential(&credential_json(
            &server.url,
            &server.vault.to_string(),
            &desktop_id.to_string(),
            TOKEN,
        ))
        .unwrap();
        assert_eq!(
            crate::import_credential(&state, Arc::new(wrong))
                .await
                .unwrap_err()
                .code,
            "revoked"
        );
        let credential = parse_credential(&credential_json(
            &server.url,
            &server.vault.to_string(),
            &desktop_id.to_string(),
            &desktop_token,
        ))
        .unwrap();
        crate::import_credential(&state, Arc::new(credential))
            .await
            .unwrap();
        let session = state.session().await.unwrap().unwrap();
        let key = state
            .store
            .get(&session.binding.db_key_account())
            .unwrap()
            .unwrap()
            .to_vec();

        // Reimport of the same credential keeps the database key and identity.
        let again = parse_credential(&credential_json(
            &server.url,
            &server.vault.to_string(),
            &desktop_id.to_string(),
            &desktop_token,
        ))
        .unwrap();
        crate::import_credential(&state, Arc::new(again))
            .await
            .unwrap();
        let session = state.session().await.unwrap().unwrap();
        assert_eq!(
            state
                .store
                .get(&session.binding.db_key_account())
                .unwrap()
                .unwrap()
                .to_vec(),
            key
        );

        let vault = sync::fetch_vault(
            &session.api,
            &session.binding.vault_id,
            &session.binding.device_id,
        )
        .await
        .unwrap();
        let (profile, header) = sync::vault_header(&vault).unwrap();
        crate::unlock_with(
            &state,
            &session,
            profile,
            header,
            zeroize::Zeroizing::new(PHRASE.into()),
            vault.profile_fingerprint.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            session.status.lock().unwrap().vault,
            Some(VaultSummary {
                epoch: 1,
                fingerprint: server.fingerprint.clone()
            })
        );

        until("live websocket ready and gateways discovered", 20, || {
            let status = session.status.lock().unwrap();
            status.live && status.gateways_known
        })
        .await;

        let draft = session
            .save_draft(&input(
                "draft-new",
                "",
                "hello from desktop",
                &[ADDRESS],
                "0",
            ))
            .unwrap();
        let send = routed(
            input(&draft.id, "", "", &[], &draft.revision),
            &gateway_id.to_string(),
            "sim-1",
        );
        let result = session.send_draft(&send).unwrap();
        assert!(result.accepted && result.status == "queued-local");
        until("outbox uploaded and acknowledged", 20, || {
            session.client.pending_outbox_batch(10).unwrap().is_empty()
        })
        .await;

        // Gateway executes the command once and reports Sent; the desktop learns it over WS.
        gateway.sync().await;
        let command = gateway
            .client
            .pending_commands()
            .unwrap()
            .into_iter()
            .next()
            .expect("gateway received the routed command");
        assert_eq!(command.subscription_id, "sim-1");
        assert!(matches!(
            gateway
                .client
                .begin_send_attempt(command.command_id)
                .unwrap(),
            PermitDecision::Permit(_)
        ));
        gateway
            .client
            .record_send_result(command.command_id, SendResult::Sent)
            .unwrap();
        gateway.sync().await;
        let conversation =
            openpush_client_core::ConversationId::from_str(&draft.conversation_id).unwrap();
        until("sent status delivered over the live connection", 20, || {
            session
                .client
                .messages(conversation)
                .unwrap()
                .iter()
                .any(|m| m.send_state == Some(openpush_client_core::SendState::Sent))
        })
        .await;

        // Incoming carrier SMS captured by the gateway reaches the desktop as unread.
        gateway
            .client
            .capture_incoming(IncomingSms {
                conversation_id: None,
                sender_address: ADDRESS.into(),
                body: "reply from phone".into(),
                provider_message_id: Some("p-1".into()),
                imported: false,
            })
            .unwrap();
        gateway.sync().await;
        until("incoming message applied", 20, || {
            session
                .client
                .list_conversations()
                .unwrap()
                .iter()
                .any(|c| c.unread_count >= 1)
        })
        .await;
        let (snapshot, _) = session
            .snapshot(None, crate::head(), Some(server.url.clone()))
            .unwrap();
        assert_eq!(snapshot.connection.state, "connected");
        assert!(snapshot
            .conversations
            .iter()
            .any(|c| c.preview == "reply from phone"
                || c.messages.iter().any(|m| m.body == "reply from phone")));
        assert_sanitized(&serde_json::to_string(&snapshot).unwrap(), root.path());
        assert!(
            hints.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "state hints were emitted"
        );

        // Revocation stops networking and is visible; local data stays.
        let http = reqwest::Client::new();
        http.post(format!("{}/v1/devices/{}/revoke", server.url, desktop_id.0))
            .bearer_auth(&server.owner)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        session.request_work();
        until("revocation observed", 40, || {
            session.status.lock().unwrap().revoked
        })
        .await;
        let (snapshot, _) = session.snapshot(None, crate::head(), None).unwrap();
        assert_eq!(
            (snapshot.connection.state, snapshot.connection.error_code),
            ("error", Some("revoked"))
        );
        assert!(!snapshot.conversations.is_empty());

        state.close_session().await;
        server.shutdown().await;
    }

    async fn desktop(
        server: &Server,
        device: DeviceId,
        token: &str,
    ) -> (tempfile::TempDir, AppState, Arc<Session>) {
        let root = tempfile::tempdir().unwrap();
        let state = AppState::new(
            root.path().to_path_buf(),
            Arc::new(MemoryStore::default()),
            Arc::new(|| {}),
        );
        let credential = parse_credential(&credential_json(
            &server.url,
            &server.vault.to_string(),
            &device.to_string(),
            token,
        ))
        .unwrap();
        crate::import_credential(&state, Arc::new(credential))
            .await
            .unwrap();
        let session = state.session().await.unwrap().unwrap();
        let vault = sync::fetch_vault(
            &session.api,
            &session.binding.vault_id,
            &session.binding.device_id,
        )
        .await
        .unwrap();
        let (profile, header) = sync::vault_header(&vault).unwrap();
        crate::unlock_with(
            &state,
            &session,
            profile,
            header,
            zeroize::Zeroizing::new(PHRASE.into()),
            vault.profile_fingerprint.clone(),
        )
        .await
        .unwrap();
        (root, state, session)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires TEST_DATABASE_URL plus the local SeaweedFS S3 from test.env; run with --include-ignored"]
    async fn real_server_mms_streaming_transfer_and_public_copy() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let server = start().await;
        let (sender_id, sender_token) = server.pair("device").await;
        let (receiver_id, receiver_token) = server.pair("device").await;
        let (gateway_id, gateway_token) = server.pair("gateway").await;
        reqwest::Client::new().post(format!("{}/v1/capabilities", server.url)).bearer_auth(&gateway_token)
            .json(&json!({"simulator":true,"capabilities":{"sims":[{"subscription_id":"sim-1","sms":"available","mms":"available"}]}}))
            .send().await.unwrap().error_for_status().unwrap();
        let (sender_root, sender_state, sender) = desktop(&server, sender_id, &sender_token).await;
        let (_receiver_root, receiver_state, receiver) =
            desktop(&server, receiver_id, &receiver_token).await;
        until("sender gateways discovered", 20, || {
            sender.status.lock().unwrap().gateways_known
        })
        .await;

        let png = sender_root.path().join("holiday photo.png");
        std::fs::write(&png, sample_png(300, 200)).unwrap();
        let s = sender.clone();
        let picked = sync::blocking(move || s.prepare_attachment(&png))
            .await
            .unwrap();
        let mut draft = input("draft-new", "", "photo", &[ADDRESS], "0");
        draft.attachment_ids = vec![picked.id.clone()];
        let draft = sender.save_draft(&draft).unwrap();
        let result = sender
            .send_draft(&routed(
                input(&draft.id, "", "", &[], &draft.revision),
                &gateway_id.to_string(),
                "sim-1",
            ))
            .unwrap();
        assert!(result.accepted);
        until(
            "ciphertext streamed, finalized and the held MMS uploaded",
            30,
            || {
                sender.client.pending_uploads().unwrap().is_empty()
                    && sender.client.pending_outbox_batch(10).unwrap().is_empty()
            },
        )
        .await;
        assert!(sender.transfer_errors.lock().unwrap().is_empty());

        // Another desktop downloads the ciphertext by streaming and installs it through core.
        let attachment = openpush_client_core::AttachmentId::from_str(&picked.id).unwrap();
        until("receiver downloaded and verified the media", 40, || {
            receiver
                .client
                .attachment_info(attachment)
                .is_ok_and(|info| info.state == openpush_client_core::AttachmentState::Available)
        })
        .await;
        let (snapshot, _) = receiver
            .snapshot(Some(&draft.conversation_id), crate::head(), None)
            .unwrap();
        let view = snapshot
            .conversations
            .iter()
            .flat_map(|c| &c.messages)
            .flat_map(|m| &m.attachments)
            .find(|a| a.id == picked.id)
            .unwrap();
        assert_eq!(view.state, "ready");
        assert!(view
            .preview_url
            .as_deref()
            .unwrap()
            .starts_with("data:image/png;base64,"));

        // Public copy: separate re-encoded upload; retrievable without credentials.
        let remote = sender
            .remote_ids
            .lock()
            .unwrap()
            .get(attachment)
            .expect("upload recorded the server object");
        let copy = crate::create_public_copy(&sender, attachment, "holiday photo.png", &remote)
            .await
            .unwrap();
        assert!(copy
            .url
            .starts_with(&format!("{}/file/mms-usercontent/", server.url)));
        assert!(copy.url.ends_with("/holidayphoto.png"));
        let response = reqwest::get(&copy.url).await.unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.bytes().await.unwrap();
        assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 300);

        // The gateway receives the MMS command but no permit until media is verified locally.
        let gateway_dir = tempfile::tempdir().unwrap();
        let gateway = Gateway {
            client: Client::open(
                ClientConfig {
                    database_path: gateway_dir.path().join("gateway.db"),
                    vault_id: server.vault,
                    device_id: gateway_id,
                },
                DatabaseKey::new(&[4; 32]).unwrap(),
            )
            .unwrap(),
            token: gateway_token,
            url: server.url.clone(),
            _dir: gateway_dir,
        };
        gateway
            .client
            .unlock(&server.profile, &server.header, PHRASE)
            .unwrap();
        gateway.sync().await;
        let command = gateway
            .client
            .pending_commands()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(matches!(
            gateway
                .client
                .begin_send_attempt(command.command_id)
                .unwrap(),
            PermitDecision::Blocked(openpush_client_core::PermitBlock::MediaUnavailable)
        ));

        sender_state.close_session().await;
        receiver_state.close_session().await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires TEST_DATABASE_URL (local PostgreSQL); run with --include-ignored"]
    async fn real_server_expired_cursor_resyncs_by_staged_snapshot_then_goes_live() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let server = start().await;
        let (first_id, first_token) = server.pair("device").await;
        let (late_id, late_token) = server.pair("device").await;
        let (gateway_id, gateway_token) = server.pair("gateway").await;
        reqwest::Client::new().post(format!("{}/v1/capabilities", server.url)).bearer_auth(&gateway_token)
            .json(&json!({"simulator":true,"capabilities":{"sims":[{"subscription_id":"sim-1","sms":"available","mms":"unsupported"}]}}))
            .send().await.unwrap().error_for_status().unwrap();
        let (_first_root, first_state, first) = desktop(&server, first_id, &first_token).await;
        until("gateways discovered", 20, || {
            first.status.lock().unwrap().gateways_known
        })
        .await;
        let draft = first
            .save_draft(&input("draft-new", "", "history one", &[ADDRESS], "0"))
            .unwrap();
        assert!(
            first
                .send_draft(&routed(
                    input(&draft.id, "", "", &[], &draft.revision),
                    &gateway_id.to_string(),
                    "sim-1"
                ))
                .unwrap()
                .accepted
        );
        until("history uploaded", 20, || {
            first.client.pending_outbox_batch(10).unwrap().is_empty()
        })
        .await;
        first_state.close_session().await;

        // Expire the whole transport log; immutable encrypted records remain for snapshots.
        sqlx::query("UPDATE event_log SET created_at=now()-interval '40 days' WHERE vault_id=$1")
            .bind(server.vault.0)
            .execute(&server.pool)
            .await
            .unwrap();
        assert!(
            openpush_server::api::prune_replay_log(&server.pool, Duration::from_secs(30 * 86_400))
                .await
                .unwrap()
                > 0
        );

        // A late device's replay from cursor 0 now gets 409 resync_required and stages a snapshot.
        let expired = reqwest::Client::new()
            .get(format!("{}/v1/events?after=0", server.url))
            .bearer_auth(&late_token)
            .send()
            .await
            .unwrap();
        assert_eq!(expired.status(), 409);
        let (_late_root, late_state, late) = desktop(&server, late_id, &late_token).await;
        let conversation =
            openpush_client_core::ConversationId::from_str(&draft.conversation_id).unwrap();
        until("snapshot history drained and applied", 30, || {
            late.client
                .messages(conversation)
                .unwrap()
                .iter()
                .any(|m| m.payload.body == "history one")
        })
        .await;
        assert!(
            !late.client.restore_guarded().unwrap(),
            "a desktop resync never sets the gateway restore guard"
        );
        until("live after resync", 20, || late.status.lock().unwrap().live).await;

        // New live traffic after the snapshot high-water applies over the socket.
        let gateway_dir = tempfile::tempdir().unwrap();
        let gateway = Gateway {
            client: Client::open(
                ClientConfig {
                    database_path: gateway_dir.path().join("gateway.db"),
                    vault_id: server.vault,
                    device_id: gateway_id,
                },
                DatabaseKey::new(&[5; 32]).unwrap(),
            )
            .unwrap(),
            token: gateway_token,
            url: server.url.clone(),
            _dir: gateway_dir,
        };
        gateway
            .client
            .unlock(&server.profile, &server.header, PHRASE)
            .unwrap();
        gateway
            .client
            .capture_incoming(IncomingSms {
                conversation_id: None,
                sender_address: "+15555550999".into(),
                body: "after resync".into(),
                provider_message_id: Some("p-9".into()),
                imported: false,
            })
            .unwrap();
        let http = reqwest::Client::new();
        for envelope in gateway.client.pending_outbox().unwrap() {
            http.post(format!("{}/v1/events", server.url))
                .bearer_auth(&gateway.token)
                .json(&envelope)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
        }
        until("live event after the snapshot applied", 20, || {
            late.client
                .list_conversations()
                .unwrap()
                .iter()
                .any(|c| c.unread_count == 1)
        })
        .await;
        late_state.close_session().await;
        server.shutdown().await;
    }

    /// A received (not self-uploaded) MMS image can be published from another desktop of the
    /// same vault through the explicit native path: the receiving desktop learns the server
    /// object ID from the bounded download metadata, re-encodes a separate copy, and the server
    /// authorizes creation by same-vault access. The private original stays private.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires TEST_DATABASE_URL plus the local SeaweedFS S3 from test.env; run with --include-ignored"]
    async fn real_server_received_image_can_be_published_as_a_separate_copy() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let server = start().await;
        let (sender_id, sender_token) = server.pair("device").await;
        let (receiver_id, receiver_token) = server.pair("device").await;
        let (gateway_id, gateway_token) = server.pair("gateway").await;
        reqwest::Client::new().post(format!("{}/v1/capabilities", server.url)).bearer_auth(&gateway_token)
            .json(&json!({"simulator":true,"capabilities":{"sims":[{"subscription_id":"sim-1","sms":"available","mms":"available"}]}}))
            .send().await.unwrap().error_for_status().unwrap();
        let (sender_root, sender_state, sender) = desktop(&server, sender_id, &sender_token).await;
        let (_receiver_root, receiver_state, receiver) =
            desktop(&server, receiver_id, &receiver_token).await;
        until("sender gateways discovered", 20, || {
            sender.status.lock().unwrap().gateways_known
        })
        .await;

        let png = sender_root.path().join("received photo.png");
        std::fs::write(&png, sample_png(120, 90)).unwrap();
        let s = sender.clone();
        let picked = sync::blocking(move || s.prepare_attachment(&png))
            .await
            .unwrap();
        let mut draft = input("draft-new", "", "photo", &[ADDRESS], "0");
        draft.attachment_ids = vec![picked.id.clone()];
        let draft = sender.save_draft(&draft).unwrap();
        assert!(
            sender
                .send_draft(&routed(
                    input(&draft.id, "", "", &[], &draft.revision),
                    &gateway_id.to_string(),
                    "sim-1"
                ))
                .unwrap()
                .accepted
        );
        until("sender uploaded the private ciphertext", 30, || {
            sender.client.pending_uploads().unwrap().is_empty()
                && sender.client.pending_outbox_batch(10).unwrap().is_empty()
        })
        .await;

        let attachment = openpush_client_core::AttachmentId::from_str(&picked.id).unwrap();
        until(
            "receiver downloaded and verified the received image",
            40,
            || {
                receiver
                    .client
                    .attachment_info(attachment)
                    .is_ok_and(|info| {
                        info.state == openpush_client_core::AttachmentState::Available
                    })
            },
        )
        .await;
        // The receiving desktop never uploaded this object; its server ID came from the
        // authenticated download metadata.
        let remote = receiver
            .remote_ids
            .lock()
            .unwrap()
            .get(attachment)
            .expect("download recorded the server object ID");
        assert_eq!(
            Some(remote.clone()),
            sender.remote_ids.lock().unwrap().get(attachment)
        );

        // Explicit native publication path (the command adds the native confirmation dialog).
        let info = receiver.client.attachment_info(attachment).unwrap();
        let prepared = crate::prepare_public_copy(&receiver, attachment, &info.display_name)
            .await
            .unwrap();
        assert!(
            crate::public_copy_prompt(&info.display_name, &prepared).contains("receivedphoto.png")
        );
        let copy = crate::upload_public_copy(&receiver, &remote, prepared)
            .await
            .expect("same-vault receiver may create a separate public copy");
        assert!(copy.url.ends_with("/receivedphoto.png"));
        let public = reqwest::get(&copy.url).await.unwrap();
        assert_eq!(public.status(), 200);
        let public = public.bytes().await.unwrap();
        let decoded = image::load_from_memory(&public).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (120, 90));

        // The original stays private ciphertext: unauthenticated access is refused, and the public
        // bytes are a different object from the private ciphertext.
        let anonymous = reqwest::get(format!("{}/v1/attachments/{remote}", server.url))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), 401);
        let private = reqwest::Client::new()
            .get(format!("{}/v1/attachments/{remote}", server.url))
            .bearer_auth(&receiver_token)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_ne!(private.as_ref(), public.as_ref());
        assert!(
            image::load_from_memory(&private).is_err(),
            "private object is ciphertext, not an image"
        );

        sender_state.close_session().await;
        receiver_state.close_session().await;
        server.shutdown().await;
    }
}
