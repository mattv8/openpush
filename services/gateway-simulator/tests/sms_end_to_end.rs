//! SIMULATED acceptance tests: real PostgreSQL, TCP HTTP/WS, SQLCipher clients and crypto.
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use peppy_client_core::*;
use peppy_crypto::{create_vault_check_header, derive_root_key};
use peppy_gateway_simulator::{
    drain_media, effects_path, publish_capabilities, run_one_carrier_effect,
    run_one_carrier_effect_for_route, sync_http, upload_pending,
};
use peppy_protocol::pairing_proof_message;
use peppy_server::api::{create_owner, router};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{fs, future::Future, path::PathBuf, time::Duration};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, client::IntoClientRequest},
};
use url::Url;
use uuid::Uuid;

const PHRASE: &str = "correct horse battery staple";
const ROTATED_PHRASE: &str = "different manual rotation phrase";
const ADDRESS: &str = "+15555550100";
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);

async fn network<T>(phase: &'static str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(NETWORK_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("phase={phase} status=timeout"))
}

struct Server {
    pool: PgPool,
    admin: PgPool,
    schema: String,
    url: String,
    ws: String,
    task: JoinHandle<()>,
    owner: String,
    vault: VaultId,
    profile: KeyProfile,
    header: VaultCheckHeader,
    fingerprint: String,
}
struct Paired {
    id: DeviceId,
    token: String,
}

impl Server {
    async fn start() -> Self {
        let database_url = std::env::var("TEST_DATABASE_URL").expect(
            "TEST_DATABASE_URL required; source .opencode/sessions/messaging-foundation/test.env",
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .unwrap();
        let schema = format!("peppy_simulator_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut isolated: Url = database_url.parse().unwrap();
        isolated
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(isolated.as_str())
            .await
            .unwrap();
        sqlx::migrate!("../server/migrations")
            .run(&pool)
            .await
            .unwrap();
        let vault = VaultId::new();
        let profile = KeyProfile::new(vault.0, 1).unwrap();
        let fingerprint = profile.fingerprint().unwrap();
        let root = derive_root_key(PHRASE, &profile).unwrap();
        let header = create_vault_check_header(&root, profile.clone()).unwrap();
        let owner = create_owner(
            &pool,
            serde_json::to_value(&profile).unwrap(),
            serde_json::to_vec(&header).unwrap(),
            fingerprint.clone(),
            1,
        )
        .await
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_pool = pool.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router(server_pool)).await.unwrap();
        });
        Self {
            pool,
            admin,
            schema,
            url: format!("http://{addr}"),
            ws: format!("ws://{addr}/v1/ws"),
            task,
            owner: owner.device_token,
            vault,
            profile,
            header,
            fingerprint,
        }
    }
    async fn pair(&self, role: &str) -> Paired {
        let key = SigningKey::from_bytes(&[9; 32]);
        let id = DeviceId::new();
        let public =
            json!({"ed25519_public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())});
        let http = reqwest::Client::builder()
            .timeout(NETWORK_TIMEOUT)
            .build()
            .unwrap();
        let challenge: Value = network("E01-pair-create", http.post(format!("{}/v1/pairing", self.url)).bearer_auth(&self.owner).json(&json!({"device_id":id.0,"public_key":public,"profile_fingerprint":self.fingerprint,"key_epoch":1,"requested_role":role})).send()).await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let challenge_bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(challenge["challenge_token"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let proof =
            pairing_proof_message(&challenge_bytes, self.vault, id, &self.fingerprint, 1, role);
        let body: Value = network("E01-pair-consume", http.post(format!("{}/v1/pairing/consume", self.url)).json(&json!({"challenge_token":challenge["challenge_token"],"device_id":id.0,"public_key":public,"profile_fingerprint":self.fingerprint,"key_epoch":1,"signature":URL_SAFE_NO_PAD.encode(key.sign(&proof).to_bytes())})).send()).await.unwrap().error_for_status().unwrap().json().await.unwrap();
        Paired {
            id,
            token: body["device_token"].as_str().unwrap().into(),
        }
    }
    async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
        network(
            "E06-cleanup",
            sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
                .execute(&self.admin),
        )
        .await
        .unwrap();
    }
}
fn client(dir: &TempDir, name: &str, s: &Server, device: DeviceId) -> Client {
    let c = Client::open(
        ClientConfig {
            database_path: dir.path().join(format!("{name}.db")),
            vault_id: s.vault,
            device_id: device,
        },
        DatabaseKey::new(&[name.as_bytes()[0]; 32]).unwrap(),
    )
    .unwrap();
    c.unlock(&s.profile, &s.header, PHRASE).unwrap();
    c
}
fn route(id: DeviceId) -> GatewayRoute {
    GatewayRoute {
        gateway_device_id: id,
        subscription_id: "sim-1".into(),
    }
}

async fn post_event(
    phase: &'static str,
    server: &Server,
    token: &str,
    envelope: &Envelope,
) -> (reqwest::StatusCode, Value) {
    let response = network(
        phase,
        reqwest::Client::builder()
            .timeout(NETWORK_TIMEOUT)
            .build()
            .unwrap()
            .post(format!("{}/v1/events", server.url))
            .bearer_auth(token)
            .json(envelope)
            .send(),
    )
    .await
    .unwrap();
    let status = response.status();
    let body = response.json().await.unwrap();
    (status, body)
}

fn queued_envelope(client: &Client, envelope_id: EnvelopeId) -> Envelope {
    client
        .pending_outbox()
        .unwrap()
        .into_iter()
        .find(|envelope| envelope.envelope_id == envelope_id)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulated_sms_checkpoint_over_real_postgres_http_and_ws() {
    let _guard = LOCK.lock().await;
    eprintln!("phase=E01 status=start");
    let server = Server::start().await;
    eprintln!("phase=E01 status=server_ready");
    let desktop_p = server.pair("device").await;
    eprintln!("phase=E01 status=desktop_paired");
    let gateway_p = server.pair("gateway").await;
    let temp = TempDir::new().unwrap();
    eprintln!("phase=E01 status=ok");

    // Both SQLCipher clients unlock only after actual signed owner pairing.
    eprintln!("phase=E02 status=desktop_unlock");
    let desktop = client(&temp, "desktop", &server, desktop_p.id);
    eprintln!("phase=E02 status=gateway_unlock");
    let gateway = client(&temp, "gateway", &server, gateway_p.id);
    eprintln!("phase=E02 status=wrong_phrase");
    assert_eq!(
        desktop.unlock(&server.profile, &server.header, "wrong phrase"),
        Err(Error::WrongPassphrase)
    );
    let mut substituted = server.profile.clone();
    substituted.salt[0] ^= 1;
    eprintln!("phase=E02 status=substituted_profile");
    assert_eq!(
        desktop.unlock(&substituted, &server.header, PHRASE),
        Err(Error::InvalidProfile)
    );
    eprintln!("phase=E02 status=ok");

    // A real WebSocket hint plus durable HTTP replay carries ciphertext and increments unread once.
    eprintln!("phase=E03 status=ws_connect");
    let mut request = server.ws.clone().into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", desktop_p.token).parse().unwrap(),
    );
    let (mut socket, _) = network("E03-ws-connect", connect_async(request))
        .await
        .unwrap();
    network(
        "E03-ws-hello",
        socket.send(tungstenite::Message::Text(
            "{\"protocol_version\":1,\"resume_cursor\":\"0\"}".into(),
        )),
    )
    .await
    .unwrap();
    let captured = gateway
        .capture_incoming(IncomingSms {
            conversation_id: None,
            sender_address: ADDRESS.into(),
            body: "SIMULATED inbound secret".into(),
            provider_message_id: Some("provider-1".into()),
            imported: false,
        })
        .unwrap();
    assert!(!captured.duplicate);
    assert!(
        serde_json::to_string(&gateway.pending_outbox().unwrap())
            .unwrap()
            .contains("ciphertext")
    );
    assert!(
        !serde_json::to_string(&gateway.pending_outbox().unwrap())
            .unwrap()
            .contains("SIMULATED inbound secret")
    );
    eprintln!("phase=E03 status=upload");
    assert_eq!(
        network(
            "E03-upload",
            upload_pending(&gateway, &server.url, &gateway_p.token)
        )
        .await
        .unwrap(),
        1
    );
    let ready: Value = serde_json::from_str(
        &network("E03-ws-ready", socket.next())
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(ready["type"], "ready");
    let frame: Value = serde_json::from_str(
        &network("E03-ws-event", socket.next())
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(frame["type"], "event");
    assert_eq!(frame["cursor"], "1");
    assert_eq!(
        frame["envelope"]["producer_device_id"],
        gateway_p.id.0.to_string()
    );
    assert!(frame["envelope"]["ciphertext"].as_str().is_some());
    assert_eq!(
        network(
            "E03-sync-first",
            sync_http(&desktop, &server.url, &desktop_p.token)
        )
        .await
        .unwrap(),
        1
    );
    assert_eq!(desktop.unread_count(captured.conversation_id).unwrap(), 1);
    assert_eq!(
        network(
            "E03-sync-replay",
            sync_http(&desktop, &server.url, &desktop_p.token)
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(desktop.unread_count(captured.conversation_id).unwrap(), 1);
    eprintln!("phase=E03 status=ok");

    // Disconnected replay/duplicate delivery and read-before-message preserve unread and local drafts.
    eprintln!("phase=E04 status=start");
    desktop
        .save_draft(captured.conversation_id, "draft survives", 0)
        .unwrap();
    desktop.mark_seen(captured.message_id).unwrap();
    network(
        "E04-upload",
        upload_pending(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    network(
        "E04-gateway-sync",
        sync_http(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    network(
        "E04-desktop-sync",
        sync_http(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    assert_eq!(desktop.unread_count(captured.conversation_id).unwrap(), 0);
    assert_eq!(
        desktop
            .draft(captured.conversation_id)
            .unwrap()
            .unwrap()
            .content,
        "draft survives"
    );
    eprintln!("phase=E04 status=ok");

    // Accepted command -> one fsynced fake effect -> encrypted result.
    eprintln!("phase=E05 status=start");
    let conversation = ConversationId::new();
    desktop
        .queue_send(
            OutgoingSms {
                conversation_id: conversation,
                recipients: vec![ADDRESS.into()],
                body: "SIMULATED outgoing secret".into(),
            },
            route(gateway_p.id),
        )
        .unwrap();
    network(
        "E05-command-upload",
        upload_pending(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    network(
        "E05-command-sync",
        sync_http(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    let effects = effects_path(temp.path());
    assert!(run_one_carrier_effect(&gateway, &effects, false).unwrap());
    assert!(!run_one_carrier_effect(&gateway, &effects, false).unwrap());
    network(
        "E05-result-upload",
        upload_pending(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    network(
        "E05-result-sync",
        sync_http(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    assert_eq!(
        desktop.messages(conversation).unwrap()[0].send_state,
        Some(SendState::Sent)
    );
    eprintln!("phase=E05 status=sent_ok");

    // Crash after effect becomes unknown and cannot duplicate.
    let second = desktop
        .queue_send(
            OutgoingSms {
                conversation_id: conversation,
                recipients: vec![ADDRESS.into()],
                body: "SIMULATED crash boundary".into(),
            },
            route(gateway_p.id),
        )
        .unwrap();
    network(
        "E05-crash-upload",
        upload_pending(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    network(
        "E05-crash-sync",
        sync_http(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    assert!(run_one_carrier_effect(&gateway, &effects, true).unwrap());
    drop(gateway);
    eprintln!("phase=E05 status=reopen");
    let reopened = client(&temp, "gateway", &server, gateway_p.id);
    assert_eq!(
        reopened.begin_send_attempt(second.command_id).unwrap(),
        PermitDecision::AlreadyAttempted(SendState::OutcomeUnknown)
    );
    assert!(!run_one_carrier_effect(&reopened, &effects, false).unwrap());
    assert_eq!(std::fs::read_to_string(effects).unwrap().lines().count(), 2);
    eprintln!("phase=E05 status=ok");

    drop(socket);
    drop(reopened);
    drop(desktop);
    eprintln!("phase=E06 status=start");
    server.shutdown().await;
    eprintln!("phase=E06 status=ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulated_mms_storage_and_public_copy_over_real_postgres_and_seaweed() {
    let _guard = LOCK.lock().await;
    eprintln!("phase=EM01 status=start");
    let server = Server::start().await;
    let desktop_p = server.pair("device").await;
    let gateway_p = server.pair("gateway").await;
    let temp = TempDir::new().unwrap();
    let desktop = client(&temp, "desktop-media", &server, desktop_p.id);
    let gateway = client(&temp, "gateway-media", &server, gateway_p.id);
    let png = fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media/valid-1x1.png"),
    )
    .unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

    // Incoming MMS: core encrypts a real PNG, media reaches server first, then encrypted metadata
    // produces exactly one unread record even while the desktop media is pending.
    let gateway_source = temp.path().join("gateway-valid.png");
    fs::write(&gateway_source, &png).unwrap();
    let inbound_attachment = gateway
        .prepare_attachment(&gateway_source, "image/png", "../incoming.png")
        .unwrap();
    let inbound = gateway
        .capture_incoming_mms(IncomingMms {
            conversation_id: None,
            sender_address: ADDRESS.into(),
            body: "real image".into(),
            recipients: Vec::new(),
            subject: None,
            provider_message_id: Some("mms-provider-1".into()),
            imported: false,
            attachment_ids: vec![inbound_attachment.attachment_id],
        })
        .unwrap();
    assert!(
        gateway.pending_outbox().unwrap().is_empty(),
        "MMS is held until upload"
    );
    network(
        "EM01-upload",
        drain_media(&gateway, &server.url, &gateway_p.token, temp.path()),
    )
    .await
    .unwrap();
    network(
        "EM01-event-upload",
        upload_pending(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    network(
        "EM01-desktop-metadata",
        sync_http(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    assert_eq!(desktop.unread_count(inbound.conversation_id).unwrap(), 1);
    let pending = desktop.pending_downloads().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        desktop
            .attachment_info(inbound_attachment.attachment_id)
            .unwrap()
            .state,
        AttachmentState::PendingDownload
    );

    // A real authenticated GET whose ciphertext is corrupt/truncated is rejected by core and cannot
    // promote media or inflate unread. The subsequent real GET/install decrypts exact PNG bytes.
    let remote = pending[0].remote_object_id.clone().unwrap();
    let cipher = network(
        "EM01-private-get",
        reqwest::Client::new()
            .get(format!("{}/v1/attachments/{remote}", server.url))
            .bearer_auth(&desktop_p.token)
            .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .bytes()
    .await
    .unwrap();
    let corrupt = temp.path().join("corrupt.ppss");
    fs::write(&corrupt, &cipher[..cipher.len() - 1]).unwrap();
    assert_eq!(
        desktop.install_downloaded_attachment(inbound_attachment.attachment_id, &corrupt),
        Err(Error::InvalidMedia)
    );
    assert_eq!(desktop.unread_count(inbound.conversation_id).unwrap(), 1);
    network(
        "EM01-install",
        drain_media(&desktop, &server.url, &desktop_p.token, temp.path()),
    )
    .await
    .unwrap();
    let plaintext = desktop
        .open_native_plaintext(inbound_attachment.attachment_id)
        .unwrap();
    assert_eq!(fs::read(plaintext.path()).unwrap(), png);
    drop(plaintext);
    assert_eq!(desktop.unread_count(inbound.conversation_id).unwrap(), 1);
    network(
        "EM01-duplicate-replay",
        sync_http(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    assert_eq!(desktop.unread_count(inbound.conversation_id).unwrap(), 1);

    // Outgoing MMS: an atomic compose draft remains held, then the real gateway cannot receive a
    // permit until its authenticated download verifies the media.
    let outbound_source = temp.path().join("desktop-valid.png");
    fs::write(&outbound_source, &png).unwrap();
    let outbound_attachment = desktop
        .prepare_attachment(&outbound_source, "image/png", "share.png")
        .unwrap();
    let draft = desktop.create_compose_draft(None).unwrap();
    desktop
        .save_compose_draft(
            draft.draft_id,
            0,
            ComposeDraftUpdate {
                text: "outbound image".into(),
                recipients: vec![ADDRESS.into()],
                attachment_ids: vec![outbound_attachment.attachment_id],
                route: Some(route(gateway_p.id)),
            },
        )
        .unwrap();
    let queued = desktop.send_compose_draft(draft.draft_id, 1).unwrap();
    assert!(desktop.pending_outbox().unwrap().is_empty());
    network(
        "EM02-upload",
        drain_media(&desktop, &server.url, &desktop_p.token, temp.path()),
    )
    .await
    .unwrap();
    network(
        "EM02-command-upload",
        upload_pending(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    network(
        "EM02-gateway-metadata",
        sync_http(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    assert_eq!(
        gateway.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::MediaUnavailable)
    );
    network(
        "EM02-gateway-install",
        drain_media(&gateway, &server.url, &gateway_p.token, temp.path()),
    )
    .await
    .unwrap();
    assert!(run_one_carrier_effect(&gateway, &effects_path(temp.path()), false).unwrap());
    network(
        "EM02-status-upload",
        upload_pending(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    network(
        "EM02-status-sync",
        sync_http(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    assert_eq!(
        desktop.messages(draft.conversation_id).unwrap()[0].send_state,
        Some(SendState::Sent)
    );

    // Public sharing is a distinct explicit plaintext derivative. The original endpoint still
    // rejects unauthenticated reads; the token works once and revocation removes only the copy.
    let share_plain = desktop
        .open_native_plaintext(outbound_attachment.attachment_id)
        .unwrap();
    let share: Value = network(
        "EM03-public-create",
        reqwest::Client::new()
            .post(format!(
                "{}/v1/attachments/{}/public-copies",
                server.url, outbound_attachment.attachment_id.0
            ))
            .bearer_auth(&desktop_p.token)
            .header("X-File-Name", "published.png")
            .body(fs::read(share_plain.path()).unwrap())
            .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    drop(share_plain);
    let token = share["token"].as_str().unwrap();
    let safe_name = share["safe_name"].as_str().unwrap();
    let share_id = share["share_id"].as_str().unwrap();
    let public = network(
        "EM03-public-get",
        reqwest::Client::new()
            .get(format!(
                "{}/file/mms-usercontent/{token}/{safe_name}",
                server.url
            ))
            .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .bytes()
    .await
    .unwrap();
    assert_eq!(public.as_ref(), png.as_slice());
    assert_eq!(
        network(
            "EM03-private-denied",
            reqwest::Client::new()
                .get(format!(
                    "{}/v1/attachments/{}",
                    server.url, outbound_attachment.attachment_id.0
                ))
                .send()
        )
        .await
        .unwrap()
        .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert!(
        network(
            "EM03-revoke",
            reqwest::Client::new()
                .post(format!("{}/v1/public-copies/{share_id}/revoke", server.url))
                .bearer_auth(&desktop_p.token)
                .send()
        )
        .await
        .unwrap()
        .status()
        .is_success()
    );
    assert_eq!(
        network(
            "EM03-revoked",
            reqwest::Client::new()
                .get(format!(
                    "{}/file/mms-usercontent/{token}/{safe_name}",
                    server.url
                ))
                .send()
        )
        .await
        .unwrap()
        .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    eprintln!("phase=EM03 status=ok");
    drop(gateway);
    drop(desktop);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulator_resyncs_pruned_replay_without_executing_snapshot_commands() {
    let _guard = LOCK.lock().await;
    eprintln!("phase=ES01 status=start");
    let server = Server::start().await;
    let desktop_p = server.pair("device").await;
    let gateway_p = server.pair("gateway").await;
    let temp = TempDir::new().unwrap();
    let desktop = client(&temp, "desktop-snapshot", &server, desktop_p.id);
    let gateway = client(&temp, "gateway-snapshot", &server, gateway_p.id);
    let conversation = ConversationId::new();
    desktop
        .save_draft(conversation, "offline draft", 0)
        .unwrap();

    // Put a real command addressed to this gateway into immutable server history, but do not let
    // the gateway receive it through the live log. It must first enter the gateway ledger through
    // the production snapshot route and be marked historical by client-core.
    let historical = desktop
        .queue_send(
            OutgoingSms {
                conversation_id: conversation,
                recipients: vec![ADDRESS.into()],
                body: "historical command".into(),
            },
            route(gateway_p.id),
        )
        .unwrap();
    network(
        "ES01-command-upload",
        upload_pending(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();

    // Keep distinct local-only work pending while the transport history is pruned and restored.
    let local = desktop
        .queue_send(
            OutgoingSms {
                conversation_id: conversation,
                recipients: vec![ADDRESS.into()],
                body: "offline outbox".into(),
            },
            route(gateway_p.id),
        )
        .unwrap();
    assert_eq!(
        desktop
            .messages(conversation)
            .unwrap()
            .last()
            .unwrap()
            .send_state,
        Some(SendState::QueuedLocal)
    );
    assert_eq!(
        desktop.begin_send_attempt(local.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::NotReceived)
    );

    // Create another immutable history record then prune both transport-log entries. The next
    // durable replay is a real 409 and sync_http must page the real snapshot route.
    let pruned = gateway
        .capture_incoming(IncomingSms {
            conversation_id: None,
            sender_address: ADDRESS.into(),
            body: "pruned history".into(),
            provider_message_id: Some("snapshot-provider".into()),
            imported: false,
        })
        .unwrap();
    network(
        "ES01-history-upload",
        upload_pending(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    assert_eq!(
        peppy_server::api::prune_replay_log(&server.pool, Duration::ZERO)
            .await
            .unwrap(),
        2
    );
    network(
        "ES01-desktop-resync",
        sync_http(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    network(
        "ES01-gateway-resync",
        sync_http(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    assert_eq!(desktop.receive_cursor().unwrap(), Cursor(2));
    assert_eq!(gateway.receive_cursor().unwrap(), Cursor(2));
    assert_eq!(
        desktop.messages(pruned.conversation_id).unwrap()[0]
            .payload
            .body,
        "pruned history"
    );
    assert_eq!(
        desktop.draft(conversation).unwrap().unwrap().content,
        "offline draft"
    );
    assert_eq!(
        desktop
            .messages(conversation)
            .unwrap()
            .last()
            .unwrap()
            .send_state,
        Some(SendState::QueuedLocal)
    );
    assert_eq!(
        desktop.pending_outbox().unwrap()[0].envelope_id,
        local.envelope_id
    );
    assert_eq!(
        gateway.begin_send_attempt(historical.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::Historical)
    );

    // Live tail after snapshot still applies. Once the real gateway is restore-guarded, the
    // genuinely received historical command cannot become a permit or a carrier side effect.
    gateway
        .capture_incoming(IncomingSms {
            conversation_id: None,
            sender_address: ADDRESS.into(),
            body: "live tail".into(),
            provider_message_id: Some("snapshot-tail".into()),
            imported: false,
        })
        .unwrap();
    network(
        "ES01-tail-upload",
        upload_pending(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    assert_eq!(
        network(
            "ES01-tail-sync",
            sync_http(&desktop, &server.url, &desktop_p.token)
        )
        .await
        .unwrap(),
        1
    );
    gateway.mark_restored_gateway().unwrap();
    assert!(gateway.restore_guarded().unwrap());
    assert_eq!(
        gateway.begin_send_attempt(historical.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::RestoreGuarded)
    );
    let effects = effects_path(temp.path());
    assert!(!run_one_carrier_effect(&gateway, &effects, false).unwrap());
    assert!(
        !effects.exists(),
        "restore-guarded history must not append a carrier effect"
    );
    assert!(
        gateway
            .commands_needing_reconciliation()
            .unwrap()
            .iter()
            .any(|item| item.command_id == historical.command_id
                && item.reason == ReconciliationReason::Historical)
    );
    eprintln!("phase=ES01 status=ok");
    drop(gateway);
    drop(desktop);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertised_sim_route_is_discoverable_and_foreign_route_never_effects() {
    let _guard = LOCK.lock().await;
    let server = Server::start().await;
    let desktop_p = server.pair("device").await;
    let gateway_p = server.pair("gateway").await;
    let temp = TempDir::new().unwrap();
    let desktop = client(&temp, "desktop-route", &server, desktop_p.id);
    let gateway = client(&temp, "gateway-route", &server, gateway_p.id);
    let route = "simulated-route-1";
    network(
        "ER01-capabilities",
        publish_capabilities(&server.url, &gateway_p.token, route),
    )
    .await
    .unwrap();
    let capabilities: Value = network(
        "ER01-discover",
        reqwest::Client::new()
            .get(format!("{}/v1/capabilities", server.url))
            .bearer_auth(&desktop_p.token)
            .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    assert!(capabilities["capabilities"].as_array().unwrap().iter().any(
        |entry| entry["device_id"] == gateway_p.id.0.to_string()
            && entry["simulator"] == true
            && entry["capabilities"]["sims"][0]["subscription_id"] == route
            && entry["capabilities"]["sims"][0]["mms_content_version"]
                == peppy_client_core::MMS_CONTENT_VERSION
    ));
    let foreign = desktop
        .queue_send(
            OutgoingSms {
                conversation_id: ConversationId::new(),
                recipients: vec![ADDRESS.into()],
                body: "foreign simulated SIM".into(),
            },
            GatewayRoute {
                gateway_device_id: gateway_p.id,
                subscription_id: "other-sim".into(),
            },
        )
        .unwrap();
    network(
        "ER01-upload",
        upload_pending(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    network(
        "ER01-sync",
        sync_http(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    let effects = effects_path(temp.path());
    assert!(!run_one_carrier_effect_for_route(&gateway, &effects, route, false).unwrap());
    assert!(!effects.exists());
    assert!(matches!(
        gateway.begin_send_attempt(foreign.command_id).unwrap(),
        PermitDecision::Permit(_)
    ));
    // The explicit direct permit above proves routing, but runtime fencing remains the only path
    // allowed to invoke the simulated carrier effect; do not append an effect for the other SIM.
    assert!(!effects.exists());
    drop(gateway);
    drop(desktop);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_passphrase_rotation_retires_old_commands_and_revokes_gateway() {
    let _guard = LOCK.lock().await;
    eprintln!("phase=EROT01 status=start");
    let server = Server::start().await;
    let desktop_p = server.pair("device").await;
    let gateway_p = server.pair("gateway").await;
    let temp = TempDir::new().unwrap();
    let desktop = client(&temp, "desktop-rotation", &server, desktop_p.id);
    let gateway = client(&temp, "gateway-rotation", &server, gateway_p.id);
    let conversation = ConversationId::new();

    // Epoch 1 is accepted before rotation. Keep the exact persisted envelope to prove an accepted
    // retry remains idempotent after its epoch retires.
    let old = desktop
        .queue_send(
            OutgoingSms {
                conversation_id: conversation,
                recipients: vec![ADDRESS.into()],
                body: "epoch one history".into(),
            },
            route(gateway_p.id),
        )
        .unwrap();
    let old_envelope = queued_envelope(&desktop, old.envelope_id);
    let (status, accepted) = post_event(
        "EROT01-old-accepted",
        &server,
        &desktop_p.token,
        &old_envelope,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(accepted["duplicate"], false);
    let accepted_cursor = accepted["cursor"].clone();
    desktop.ack_outbox(old.envelope_id).unwrap();

    // This distinct epoch-1 command is sealed before cutover but first submitted after cutover.
    let retired = desktop
        .queue_send(
            OutgoingSms {
                conversation_id: conversation,
                recipients: vec![ADDRESS.into()],
                body: "retired before acceptance".into(),
            },
            route(gateway_p.id),
        )
        .unwrap();
    let retired_envelope = queued_envelope(&desktop, retired.envelope_id);

    // The owner registers and activates a real epoch-2 profile/header derived locally from a
    // genuinely different manual passphrase.
    let profile_two = KeyProfile::new(server.vault.0, 2).unwrap();
    let fingerprint_two = profile_two.fingerprint().unwrap();
    let root_two = derive_root_key(ROTATED_PHRASE, &profile_two).unwrap();
    let header_two = create_vault_check_header(&root_two, profile_two.clone()).unwrap();
    let register = json!({
        "key_epoch": 2,
        "public_key_profile": profile_two,
        "encrypted_vault_check_header": STANDARD.encode(serde_json::to_vec(&header_two).unwrap()),
        "profile_fingerprint": fingerprint_two,
    });
    let http = reqwest::Client::builder()
        .timeout(NETWORK_TIMEOUT)
        .build()
        .unwrap();
    let registered = network(
        "EROT02-register",
        http.post(format!("{}/v1/vault/key-profiles", server.url))
            .bearer_auth(&server.owner)
            .json(&register)
            .send(),
    )
    .await
    .unwrap();
    assert_eq!(registered.status(), reqwest::StatusCode::CREATED);
    let activated: Value = network(
        "EROT02-activate",
        http.post(format!("{}/v1/vault/key-profiles/2/activate", server.url))
            .bearer_auth(&server.owner)
            .json(&json!({}))
            .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(activated["duplicate"], false);

    let (status, denied) = post_event(
        "EROT02-retired-denied",
        &server,
        &desktop_p.token,
        &retired_envelope,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT);
    assert_eq!(denied["code"], "retired_key_epoch");
    let (status, duplicate) = post_event(
        "EROT02-old-duplicate",
        &server,
        &desktop_p.token,
        &old_envelope,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(duplicate["cursor"], accepted_cursor);

    // Both real clients manually unlock and activate epoch 2. A failed old-passphrase check leaves
    // the pinned epoch-2 profile and active epoch unchanged.
    desktop
        .unlock(&profile_two, &header_two, ROTATED_PHRASE)
        .unwrap();
    gateway
        .unlock(&profile_two, &header_two, ROTATED_PHRASE)
        .unwrap();
    desktop.activate_epoch(2).unwrap();
    gateway.activate_epoch(2).unwrap();
    assert_eq!(
        desktop.unlock(&profile_two, &header_two, PHRASE),
        Err(Error::WrongPassphrase)
    );
    assert_eq!(desktop.key_status().unwrap().active_epoch, Some(2));
    let mut substituted_two = profile_two.clone();
    substituted_two.salt[0] ^= 1;
    let substituted_header = VaultCheckHeader {
        profile: substituted_two.clone(),
        check: header_two.check.clone(),
    };
    assert_eq!(
        desktop.unlock(&substituted_two, &substituted_header, PHRASE),
        Err(Error::InvalidProfile),
        "a failed old-passphrase check must not clear the pinned epoch-2 profile"
    );
    desktop
        .unlock(&profile_two, &header_two, ROTATED_PHRASE)
        .unwrap();
    assert_eq!(desktop.key_status().unwrap().active_epoch, Some(2));

    // Retaining epoch-1 keys permits reading old history; it is not a forward-secrecy claim. The
    // unattempted old command is stale and cannot cross the simulated carrier boundary.
    network(
        "EROT03-old-sync",
        sync_http(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    assert_eq!(
        gateway
            .messages(conversation)
            .unwrap()
            .iter()
            .find(|message| message.payload.body == "epoch one history")
            .unwrap()
            .payload
            .body,
        "epoch one history"
    );
    assert_eq!(
        gateway.begin_send_attempt(old.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::StaleEpoch)
    );
    let effects = effects_path(temp.path());
    assert!(!run_one_carrier_effect(&gateway, &effects, false).unwrap());
    assert!(!effects.exists());

    // A fresh epoch-2 command uses the normal permit/effect/status path.
    let fresh = desktop
        .queue_send(
            OutgoingSms {
                conversation_id: conversation,
                recipients: vec![ADDRESS.into()],
                body: "epoch two fresh".into(),
            },
            route(gateway_p.id),
        )
        .unwrap();
    let fresh_envelope = queued_envelope(&desktop, fresh.envelope_id);
    let (status, fresh_accepted) = post_event(
        "EROT03-fresh-accepted",
        &server,
        &desktop_p.token,
        &fresh_envelope,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{fresh_accepted}");
    desktop.ack_outbox(fresh.envelope_id).unwrap();
    network(
        "EROT03-fresh-sync",
        sync_http(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    assert!(run_one_carrier_effect(&gateway, &effects, false).unwrap());
    network(
        "EROT03-status-upload",
        upload_pending(&gateway, &server.url, &gateway_p.token),
    )
    .await
    .unwrap();
    network(
        "EROT03-status-sync",
        sync_http(&desktop, &server.url, &desktop_p.token),
    )
    .await
    .unwrap();
    assert_eq!(
        desktop
            .messages(conversation)
            .unwrap()
            .iter()
            .find(|message| message.payload.body == "epoch two fresh")
            .unwrap()
            .send_state,
        Some(SendState::Sent)
    );
    assert_eq!(fs::read_to_string(&effects).unwrap().lines().count(), 1);

    // Revocation rejects further authenticated HTTP and closes an already-authenticated socket.
    let mut request = server.ws.clone().into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", gateway_p.token).parse().unwrap(),
    );
    let (mut socket, _) = network("EROT04-ws-connect", connect_async(request))
        .await
        .unwrap();
    network(
        "EROT04-ws-hello",
        socket.send(tungstenite::Message::Text(
            json!({"protocol_version":1,"resume_cursor":gateway.receive_cursor().unwrap().0.to_string()})
                .to_string()
                .into(),
        )),
    )
    .await
    .unwrap();
    let ready: Value = serde_json::from_str(
        network("EROT04-ws-ready", socket.next())
            .await
            .unwrap()
            .unwrap()
            .to_text()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(ready["type"], "ready");

    let revoked = network(
        "EROT04-revoke",
        http.post(format!(
            "{}/v1/devices/{}/revoke",
            server.url, gateway_p.id.0
        ))
        .bearer_auth(&server.owner)
        .send(),
    )
    .await
    .unwrap();
    assert_eq!(revoked.status(), reqwest::StatusCode::NO_CONTENT);
    let denied = network(
        "EROT04-http-denied",
        http.get(format!("{}/v1/events?after=0&limit=1", server.url))
            .bearer_auth(&gateway_p.token)
            .send(),
    )
    .await
    .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
    let close = tokio::time::timeout(Duration::from_secs(22), async {
        loop {
            match socket.next().await {
                Some(Ok(tungstenite::Message::Close(frame))) => break frame,
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("revoked websocket failed before close: {error}"),
                None => panic!("revoked websocket ended without close frame"),
            }
        }
    })
    .await
    .expect("revoked websocket did not close");
    assert_eq!(
        close.unwrap().code,
        tungstenite::protocol::frame::coding::CloseCode::Library(4401)
    );

    eprintln!("phase=EROT04 status=ok");
    drop(gateway);
    drop(desktop);
    server.shutdown().await;
}
