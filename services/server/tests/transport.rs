use std::{
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use peppy_domain::{DeviceId, VaultId};
use peppy_protocol::pairing_proof_message;
use peppy_server::api::{
    TransportOptions, compact_records, create_owner, prune_expired_pairing_intents,
    prune_replay_log, router_with_options, router_with_options_and_relay,
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_tungstenite::{connect_async, tungstenite};
use url::Url;
use uuid::Uuid;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TestServer {
    pool: PgPool,
    admin: PgPool,
    schema: String,
    base_url: String,
    ws_url: String,
    task: JoinHandle<()>,
    owner_token: String,
    owner_device: Uuid,
    vault: Uuid,
    fingerprint: String,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with(TransportOptions::default()).await
    }

    async fn start_with(options: TransportOptions) -> Self {
        Self::start_custom(options, None, None).await
    }

    /// A vault whose owner profile/header are real `peppy-crypto` values, so a real
    /// `peppy_client_core::Client` (device = owner) can encrypt for it.
    async fn start_real(
        profile: &peppy_crypto::KeyProfile,
        header: &peppy_crypto::VaultCheckHeader,
    ) -> Self {
        let real = (
            serde_json::to_value(profile).unwrap(),
            profile.fingerprint().unwrap(),
            serde_json::to_vec(header).unwrap(),
            profile.vault_id,
        );
        Self::start_custom(TransportOptions::default(), Some(real), None).await
    }

    async fn start_custom(
        options: TransportOptions,
        real: Option<(Value, String, Vec<u8>, Uuid)>,
        relay_url: Option<Url>,
    ) -> Self {
        let database_url = std::env::var("TEST_DATABASE_URL")
            .expect("TEST_DATABASE_URL is required; integration tests never silently skip");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .unwrap();
        let schema = format!("peppy_server_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();

        let mut isolated_url: Url = database_url.parse().unwrap();
        isolated_url
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(isolated_url.as_str())
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        let (vault, profile, fingerprint, header) = match real {
            Some((profile, fingerprint, header, vault)) => (vault, profile, fingerprint, header),
            None => {
                let vault = Uuid::new_v4();
                let (profile, fingerprint) = profile(vault, 1);
                (vault, profile, fingerprint, vec![7, 8, 9])
            }
        };
        let owner = create_owner(&pool, profile, header, fingerprint.clone(), 1)
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_pool = pool.clone();
        let task = tokio::spawn(async move {
            let router = match relay_url {
                Some(relay_url) => router_with_options_and_relay(server_pool, options, relay_url),
                None => router_with_options(server_pool, options),
            };
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            pool,
            admin,
            schema,
            base_url: format!("http://{addr}"),
            ws_url: format!("ws://{addr}/v1/ws"),
            task,
            owner_token: owner.device_token,
            owner_device: owner.device_id,
            vault,
            fingerprint,
        }
    }

    async fn start_with_relay(relay_url: Url) -> Self {
        Self::start_custom(TransportOptions::default(), None, Some(relay_url)).await
    }

    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.bearer_auth(&self.owner_token)
    }

    async fn pair(&self, role: &str) -> PairedDevice {
        let key = SigningKey::from_bytes(&rand_bytes());
        self.pair_with_key(role, key).await
    }

    async fn pair_for(
        &self,
        owner_token: &str,
        vault: Uuid,
        fingerprint: &str,
        role: &str,
    ) -> PairedDevice {
        let key = SigningKey::from_bytes(&rand_bytes());
        let device = Uuid::new_v4();
        let public_key =
            json!({"ed25519_public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())});
        let response = Client::new()
            .post(format!("{}/v1/pairing", self.base_url))
            .bearer_auth(owner_token)
            .json(&json!({
                "device_id": device,
                "public_key": public_key,
                "profile_fingerprint": fingerprint,
                "key_epoch": 1,
                "requested_role": role,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let challenge: Value = response.json().await.unwrap();
        let token = challenge["challenge_token"].as_str().unwrap();
        let challenge_bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(token).unwrap().try_into().unwrap();
        let proof = pairing_proof_message(
            &challenge_bytes,
            VaultId(vault),
            DeviceId(device),
            fingerprint,
            1,
            role,
        );
        let response = Client::new()
            .post(format!("{}/v1/pairing/consume", self.base_url))
            .json(&json!({
                "challenge_token": token,
                "device_id": device,
                "public_key": public_key,
                "profile_fingerprint": fingerprint,
                "key_epoch": 1,
                "signature": URL_SAFE_NO_PAD.encode(key.sign(&proof).to_bytes()),
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        PairedDevice {
            id: device,
            token: body["device_token"].as_str().unwrap().to_owned(),
            role: body["role"].as_str().unwrap().to_owned(),
        }
    }

    async fn pair_with_key(&self, role: &str, key: SigningKey) -> PairedDevice {
        let device = Uuid::new_v4();
        let public_key =
            json!({"ed25519_public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())});
        let client = Client::new();
        let response = self
            .auth(client.post(format!("{}/v1/pairing", self.base_url)))
            .json(&json!({
                "device_id": device,
                "public_key": public_key,
                "profile_fingerprint": self.fingerprint,
                "key_epoch": 1,
                "requested_role": role,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let challenge: Value = response.json().await.unwrap();
        self.consume(&challenge, device, &public_key, &key, None)
            .await
    }

    async fn consume(
        &self,
        challenge: &Value,
        device: Uuid,
        public_key: &Value,
        key: &SigningKey,
        attempted_role: Option<&str>,
    ) -> PairedDevice {
        let token = challenge["challenge_token"].as_str().unwrap();
        let challenge_bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(token).unwrap().try_into().unwrap();
        let approved_role = challenge["requested_role"].as_str().unwrap();
        let message = pairing_proof_message(
            &challenge_bytes,
            VaultId(self.vault),
            DeviceId(device),
            &self.fingerprint,
            1,
            approved_role,
        );
        let signature = URL_SAFE_NO_PAD.encode(key.sign(&message).to_bytes());
        let mut body = json!({
            "challenge_token": token,
            "device_id": device,
            "public_key": public_key,
            "profile_fingerprint": self.fingerprint,
            "key_epoch": 1,
            "signature": signature,
        });
        if let Some(role) = attempted_role {
            body["requested_role"] = json!(role);
        }
        let response = Client::new()
            .post(format!("{}/v1/pairing/consume", self.base_url))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        PairedDevice {
            id: device,
            token: body["device_token"].as_str().unwrap().to_owned(),
            role: body["role"].as_str().unwrap().to_owned(),
        }
    }

    async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
        self.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

struct PairedDevice {
    id: Uuid,
    token: String,
    role: String,
}

fn rand_bytes() -> [u8; 32] {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut bytes = [0; 32];
    bytes[..16].copy_from_slice(first.as_bytes());
    bytes[16..].copy_from_slice(second.as_bytes());
    bytes
}

fn profile(vault: Uuid, epoch: u32) -> (Value, String) {
    let salt: Vec<u8> = (0..16).collect();
    let mut digest = Sha256::new();
    digest.update(b"peppy-key-profile-v1\0");
    digest.update(1_u16.to_be_bytes());
    digest.update(&salt);
    digest.update(vault.as_bytes());
    digest.update(epoch.to_be_bytes());
    (
        json!({"crypto_suite": 1, "salt": salt, "vault_id": vault, "key_epoch": epoch}),
        hex::encode(digest.finalize()),
    )
}

fn event(vault: Uuid, producer: Uuid, sequence: u64, envelope: Uuid, payload: u8) -> Value {
    json!({
        "protocol_version": 1,
        "envelope_id": envelope,
        "command_id": null,
        "vault_id": vault,
        "producer_device_id": producer,
        "producer_sequence": sequence.to_string(),
        "key_epoch": 1,
        "crypto_suite": 1,
        "profile_fingerprint": "PLACEHOLDER",
        "purpose": "event",
        "route": null,
        "ciphertext": base64::engine::general_purpose::STANDARD.encode([payload]),
    })
}

fn compacting_event(
    vault: Uuid,
    producer: Uuid,
    sequence: u64,
    envelope: Uuid,
    supersedes: Vec<(Uuid, u64)>,
) -> Value {
    let mut value = event(vault, producer, sequence, envelope, sequence as u8);
    value["compaction"] = json!({
        "key": base64::engine::general_purpose::STANDARD.encode([7_u8; 32]),
        "terminal": false,
        "supersedes": supersedes.into_iter().map(|(producer_device_id, producer_sequence)| json!({
            "producer_device_id": producer_device_id,
            "producer_sequence": producer_sequence.to_string(),
        })).collect::<Vec<_>>(),
    });
    value
}

fn command(
    vault: Uuid,
    producer: Uuid,
    sequence: u64,
    envelope: Uuid,
    command: Uuid,
    gateway: Uuid,
) -> Value {
    json!({
        "protocol_version": 1,
        "envelope_id": envelope,
        "command_id": command,
        "vault_id": vault,
        "producer_device_id": producer,
        "producer_sequence": sequence.to_string(),
        "key_epoch": 1,
        "crypto_suite": 1,
        "profile_fingerprint": "PLACEHOLDER",
        "purpose": "command",
        "route": {"gateway_device_id": gateway, "subscription_id": "sim:test"},
        "ciphertext": base64::engine::general_purpose::STANDARD.encode([9, 8, 7]),
    })
}

fn set_fingerprint(mut envelope: Value, fingerprint: &str) -> Value {
    envelope["profile_fingerprint"] = json!(fingerprint);
    envelope
}

#[derive(Clone)]
struct FakeRelay {
    status: Arc<AtomicU16>,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
    hold_response: Arc<std::sync::atomic::AtomicBool>,
    release_response: Arc<tokio::sync::Notify>,
}

async fn fake_wake(
    axum::extract::State(relay): axum::extract::State<FakeRelay>,
    axum::Json(body): axum::Json<Value>,
) -> axum::http::StatusCode {
    relay.requests.lock().await.push(body);
    if relay.hold_response.load(Ordering::SeqCst) {
        relay.release_response.notified().await;
    }
    axum::http::StatusCode::from_u16(relay.status.load(Ordering::SeqCst)).unwrap()
}

async fn start_fake_relay() -> (Url, FakeRelay, JoinHandle<()>) {
    let relay = FakeRelay {
        status: Arc::new(AtomicU16::new(202)),
        requests: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        hold_response: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = axum::Router::new()
        .route(
            "/relay/v1/routes/{route}/wake",
            axum::routing::post(fake_wake),
        )
        .with_state(relay.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (
        format!("http://{address}/relay").parse().unwrap(),
        relay,
        task,
    )
}

#[tokio::test]
async fn newer_commit_during_inflight_wake_rotates_the_delivery_hint() {
    let _guard = TEST_LOCK.lock().await;
    let (relay_url, relay, relay_task) = start_fake_relay().await;
    let server = TestServer::start_with_relay(relay_url).await;
    let gateway = server.pair("gateway").await;
    assert_eq!(
        Client::new()
            .put(format!("{}/v1/devices/self/wake-route", server.base_url))
            .bearer_auth(&gateway.token)
            .json(&json!({"route_id":Uuid::new_v4(),"wake_credential":"a".repeat(64)}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );

    relay.hold_response.store(true, Ordering::SeqCst);
    server.commit(&server.owner_event(1)).await;
    wait_until(5, "in-flight wake", || {
        let relay = relay.clone();
        async move { relay.requests.lock().await.len() == 1 }
    })
    .await;
    server.commit(&server.owner_event(2)).await;
    relay.hold_response.store(false, Ordering::SeqCst);
    relay.release_response.notify_one();
    wait_until(5, "replacement wake", || {
        let relay = relay.clone();
        async move { relay.requests.lock().await.len() == 2 }
    })
    .await;
    let requests = relay.requests.lock().await;
    assert_ne!(requests[0]["idempotency_id"], requests[1]["idempotency_id"]);
    assert_ne!(requests[0]["opaque_nonce"], requests[1]["opaque_nonce"]);
    drop(requests);
    wait_until(3, "replacement wake deletion", || {
        let pool = server.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM device_wake_jobs")
                .fetch_one(&pool)
                .await
                .unwrap()
                == 0
        }
    })
    .await;
    server.shutdown().await;
    relay_task.abort();
}

#[tokio::test]
async fn durable_wakes_use_random_opaque_ids_coalesce_and_replace_invalid_routes() {
    let _guard = TEST_LOCK.lock().await;
    let (relay_url, relay, relay_task) = start_fake_relay().await;
    let server = TestServer::start_with_relay(relay_url).await;
    let gateway = server.pair("gateway").await;
    let client = Client::new();
    let register = |route_id| {
        client
            .put(format!("{}/v1/devices/self/wake-route", server.base_url))
            .bearer_auth(&gateway.token)
            .json(&json!({"route_id":route_id,"wake_credential":"a".repeat(64)}))
    };
    assert_eq!(
        register(Uuid::new_v4()).send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    // Two committed records before the worker claims work are one durable wake.
    server.commit(&server.owner_event(1)).await;
    server.commit(&server.owner_event(2)).await;
    wait_until(5, "accepted coalesced wake", || {
        let relay = relay.clone();
        async move { relay.requests.lock().await.len() == 1 }
    })
    .await;
    let request = relay.requests.lock().await[0].clone();
    let encoded = request.to_string();
    assert!(!encoded.contains(&server.vault.to_string()));
    assert!(!encoded.contains(&gateway.id.to_string()));
    assert!(!encoded.contains("cursor"));
    assert_ne!(request["idempotency_id"], Value::Null);
    assert_ne!(request["opaque_nonce"], Value::Null);
    wait_until(3, "accepted wake deletion", || {
        let pool = server.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM device_wake_jobs")
                .fetch_one(&pool)
                .await
                .unwrap()
                == 0
        }
    })
    .await;

    // A transient response retries the immutable persisted idempotency pair.
    relay.status.store(503, Ordering::SeqCst);
    server.commit(&server.owner_event(3)).await;
    wait_until(5, "retryable wake", || {
        let relay = relay.clone();
        async move { relay.requests.lock().await.len() == 2 }
    })
    .await;
    let retry_id = relay.requests.lock().await[1]["idempotency_id"].clone();
    let retry_nonce = relay.requests.lock().await[1]["opaque_nonce"].clone();
    relay.status.store(202, Ordering::SeqCst);
    wait_until(5, "accepted wake retry", || {
        let relay = relay.clone();
        async move { relay.requests.lock().await.len() == 3 }
    })
    .await;
    assert_eq!(relay.requests.lock().await[2]["idempotency_id"], retry_id);
    assert_eq!(relay.requests.lock().await[2]["opaque_nonce"], retry_nonce);

    relay.status.store(401, Ordering::SeqCst);
    server.commit(&server.owner_event(4)).await;
    wait_until(5, "401 wake", || {
        let relay = relay.clone();
        async move { relay.requests.lock().await.len() == 4 }
    })
    .await;
    wait_until(3, "old route revocation", || {
        let pool = server.pool.clone();
        async move { sqlx::query_scalar::<_, bool>("SELECT revoked_at IS NOT NULL FROM device_wake_routes WHERE vault_id=$1 AND device_id=$2").bind(server.vault).bind(gateway.id).fetch_one(&pool).await.unwrap() }
    }).await;
    relay.status.store(202, Ordering::SeqCst);
    assert_eq!(
        register(Uuid::new_v4()).send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    server.commit(&server.owner_event(5)).await;
    wait_until(5, "replacement route wake", || {
        let relay = relay.clone();
        async move { relay.requests.lock().await.len() == 5 }
    })
    .await;
    server.shutdown().await;
    relay_task.abort();
}

#[tokio::test]
async fn owner_vault_identity_and_signed_pairing_are_real_postgres_and_tcp() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let vault = server
        .auth(Client::new().get(format!("{}/v1/vault", server.base_url)))
        .send()
        .await
        .unwrap();
    assert_eq!(vault.status(), StatusCode::OK);
    let vault: Value = vault.json().await.unwrap();
    assert_eq!(vault["vault_id"], json!(server.vault));
    assert_eq!(vault["profile_fingerprint"], json!(server.fingerprint));
    assert_eq!(vault["public_key_profile"]["vault_id"], json!(server.vault));
    assert_eq!(vault["encrypted_vault_check_header"], "BwgJ");

    let gateway = server.pair("gateway").await;
    assert_eq!(gateway.role, "gateway");
    let devices: Value = Client::new()
        .get(format!("{}/v1/devices", server.base_url))
        .bearer_auth(&gateway.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        devices["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["device_id"] == json!(gateway.id) && d["role"] == "gateway")
    );
    server.shutdown().await;
}

#[tokio::test]
async fn pairing_rejects_bad_expired_replayed_proofs_and_cannot_escalate_role() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let client = Client::new();
    let invalid_role = server.auth(client.post(format!("{}/v1/pairing", server.base_url)))
        .json(&json!({"device_id":Uuid::new_v4(),"public_key":{"ed25519_public_key":URL_SAFE_NO_PAD.encode([1;32])},"profile_fingerprint":server.fingerprint,"key_epoch":1,"requested_role":"owner"}))
        .send().await.unwrap();
    assert_eq!(invalid_role.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let key = SigningKey::from_bytes(&rand_bytes());
    let device = Uuid::new_v4();
    let public_key =
        json!({"ed25519_public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())});
    let challenge: Value = server.auth(client.post(format!("{}/v1/pairing", server.base_url)))
        .json(&json!({"device_id":device,"public_key":public_key,"profile_fingerprint":server.fingerprint,"key_epoch":1,"requested_role":"device"}))
        .send().await.unwrap().json().await.unwrap();
    let mut bad = json!({
        "challenge_token":challenge["challenge_token"],"device_id":device,"public_key":public_key,
        "profile_fingerprint":server.fingerprint,"key_epoch":1,"signature":URL_SAFE_NO_PAD.encode([0;64])
    });
    let response = client
        .post(format!("{}/v1/pairing/consume", server.base_url))
        .json(&bad)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let paired = server
        .consume(&challenge, device, &public_key, &key, Some("gateway"))
        .await;
    assert_eq!(
        paired.role, "device",
        "consumer-supplied role must not escalate approval"
    );
    let replay = client
        .post(format!("{}/v1/pairing/consume", server.base_url))
        .json(&bad)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);

    let expired_key = SigningKey::from_bytes(&rand_bytes());
    let expired_device = Uuid::new_v4();
    let expired_public = json!({"ed25519_public_key":URL_SAFE_NO_PAD.encode(expired_key.verifying_key().as_bytes())});
    let expired: Value = server.auth(client.post(format!("{}/v1/pairing", server.base_url)))
        .json(&json!({"device_id":expired_device,"public_key":expired_public,"profile_fingerprint":server.fingerprint,"key_epoch":1,"requested_role":"device"}))
        .send().await.unwrap().json().await.unwrap();
    let digest = Sha256::digest(expired["challenge_token"].as_str().unwrap().as_bytes());
    sqlx::query("UPDATE pairing_challenges SET expires_at=now()-interval '1 second' WHERE challenge_digest=$1")
        .bind(digest.as_slice()).execute(&server.pool).await.unwrap();
    let token = expired["challenge_token"].as_str().unwrap();
    let raw: [u8; 32] = URL_SAFE_NO_PAD.decode(token).unwrap().try_into().unwrap();
    let proof = pairing_proof_message(
        &raw,
        VaultId(server.vault),
        DeviceId(expired_device),
        &server.fingerprint,
        1,
        "device",
    );
    bad = json!({"challenge_token":token,"device_id":expired_device,"public_key":expired_public,"profile_fingerprint":server.fingerprint,"key_epoch":1,"signature":URL_SAFE_NO_PAD.encode(expired_key.sign(&proof).to_bytes())});
    let response = client
        .post(format!("{}/v1/pairing/consume", server.base_url))
        .json(&bad)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    server.shutdown().await;
}

#[tokio::test]
async fn pairing_intent_claim_approval_challenge_and_consume_are_end_to_end() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let client = Client::new();
    let (status, intent) = server
        .post(
            &server.owner_token,
            "/v1/pairing/intents",
            &json!({"https_origin":"https://api.example"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{intent}");
    assert_eq!(intent["https_origin"], "https://api.example");
    let intent_token = intent["intent_token"].as_str().unwrap();
    assert_eq!(intent_token.len(), 43);

    let key = SigningKey::from_bytes(&rand_bytes());
    let device = Uuid::new_v4();
    let public_key =
        json!({"ed25519_public_key":URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())});
    let claim_body = json!({
        "device_id": device,
        "public_key": public_key,
        "requested_role": "gateway",
    });
    let (status, claim) = server
        .call(
            client
                .post(format!(
                    "{}/v1/pairing/intents/{intent_token}/claim",
                    server.base_url
                ))
                .json(&claim_body),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    assert_eq!(claim["key_digest"].as_str().unwrap().len(), 64);
    assert_eq!(claim["sas"].as_str().unwrap().len(), 6);
    assert_eq!(claim["claim_secret"].as_str().unwrap().len(), 43);

    let (status, duplicate_claim) = server
        .call(
            client
                .post(format!(
                    "{}/v1/pairing/intents/{intent_token}/claim",
                    server.base_url
                ))
                .json(&claim_body),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(duplicate_claim["code"], "pairing_intent_claimed");

    let (status, intent_status) = server
        .get(
            &server.owner_token,
            &format!("/v1/pairing/intents/{intent_token}"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{intent_status}");
    assert_eq!(intent_status["claimed"], true);
    assert_eq!(intent_status["approved"], false);
    assert_eq!(intent_status["device_id"], json!(device));
    assert_eq!(intent_status["key_digest"], claim["key_digest"]);
    assert_eq!(intent_status["sas"], claim["sas"]);

    let (status, approved) = server
        .post(
            &server.owner_token,
            &format!("/v1/pairing/intents/{intent_token}/approve"),
            &json!({
                "key_digest": claim["key_digest"],
                "profile_fingerprint": server.fingerprint,
                "key_epoch": 1,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["requested_role"], "gateway");

    // Approval starts a fresh challenge lifetime. The original QR intent may
    // expire while the phone is waiting to retrieve that approved challenge.
    sqlx::query(
        "UPDATE pairing_intents SET expires_at=now()-interval '1 second' WHERE intent_digest=$1",
    )
    .bind(Sha256::digest(intent_token.as_bytes()).as_slice())
    .execute(&server.pool)
    .await
    .unwrap();

    let (status, challenge) = server
        .call(
            client
                .post(format!(
                    "{}/v1/pairing/intents/{intent_token}/challenge",
                    server.base_url
                ))
                .json(&json!({
                    "device_id": device,
                    "key_digest": claim["key_digest"],
                    "claim_secret": claim["claim_secret"],
                })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    assert_eq!(challenge["challenge_token"], approved["challenge_token"]);
    assert_eq!(challenge["vault_id"], approved["vault_id"]);
    assert_eq!(challenge["requested_role"], approved["requested_role"]);
    assert!(challenge["expires_in_seconds"].as_u64().unwrap() <= 120);

    let paired = server
        .consume(&challenge, device, &public_key, &key, None)
        .await;
    assert_eq!(paired.id, device);
    assert_eq!(paired.role, "gateway");
    let secrets_cleared: bool = sqlx::query_scalar(
        "SELECT claim_secret_digest IS NULL AND challenge_token IS NULL FROM pairing_intents WHERE intent_digest=$1",
    )
    .bind(Sha256::digest(intent_token.as_bytes()).as_slice())
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert!(secrets_cleared);

    let (status, unavailable) = server
        .call(
            client
                .post(format!(
                    "{}/v1/pairing/intents/{intent_token}/challenge",
                    server.base_url
                ))
                .json(&json!({
                    "device_id": device,
                    "key_digest": claim["key_digest"],
                    "claim_secret": claim["claim_secret"],
                })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(unavailable["code"], "pairing_challenge_unavailable");
    server.shutdown().await;
}

#[tokio::test]
async fn unauthenticated_pairing_admission_is_bounded_per_intent_without_global_throttle() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let client = Client::new();
    let valid_public_key =
        URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(&[8; 32]).verifying_key().as_bytes());
    let first = server
        .post(
            &server.owner_token,
            "/v1/pairing/intents",
            &json!({"https_origin":"https://api.example"}),
        )
        .await
        .1["intent_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = server
        .post(
            &server.owner_token,
            "/v1/pairing/intents",
            &json!({"https_origin":"https://api.example"}),
        )
        .await
        .1["intent_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let claim = |token: &str| {
        client
            .post(format!(
                "{}/v1/pairing/intents/{token}/claim",
                server.base_url
            ))
            .json(&json!({
                "device_id": Uuid::new_v4(),
                "public_key": {"ed25519_public_key": valid_public_key},
                "requested_role": "device",
            }))
    };
    assert_eq!(claim(&first).send().await.unwrap().status(), StatusCode::OK);
    // The initial successful claim consumes one of the 20 admissions.
    for _ in 0..19 {
        assert_eq!(
            claim(&first).send().await.unwrap().status(),
            StatusCode::CONFLICT
        );
    }
    assert_eq!(
        claim(&first).send().await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        claim(&second).send().await.unwrap().status(),
        StatusCode::OK
    );
    server.shutdown().await;
}

#[tokio::test]
async fn pairing_admission_capacity_evicts_random_tokens_without_blocking_valid_claims() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let client = Client::new();
    let valid_public_key =
        URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(&[9; 32]).verifying_key().as_bytes());
    let known = server
        .post(
            &server.owner_token,
            "/v1/pairing/intents",
            &json!({"https_origin":"https://api.example"}),
        )
        .await
        .1["intent_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let requests = (0..=4096).map(|_| {
        let client = client.clone();
        let base_url = server.base_url.clone();
        let valid_public_key = valid_public_key.clone();
        async move {
            client
                .post(format!(
                    "{base_url}/v1/pairing/intents/{}/claim",
                    URL_SAFE_NO_PAD.encode(rand_bytes())
                ))
                .json(&json!({
                    "device_id": Uuid::new_v4(),
                    "public_key": {"ed25519_public_key": valid_public_key},
                    "requested_role": "device",
                }))
                .send()
                .await
                .unwrap()
                .status()
        }
    });
    let statuses: Vec<StatusCode> = futures_util::stream::iter(requests)
        .buffer_unordered(16)
        .collect()
        .await;
    assert!(
        statuses
            .iter()
            .all(|status| *status == StatusCode::UNAUTHORIZED)
    );

    let claim = |token: &str| {
        client
            .post(format!(
                "{}/v1/pairing/intents/{token}/claim",
                server.base_url
            ))
            .json(&json!({
                "device_id": Uuid::new_v4(),
                "public_key": {"ed25519_public_key": valid_public_key},
                "requested_role": "device",
            }))
    };
    assert_eq!(claim(&known).send().await.unwrap().status(), StatusCode::OK);
    let new = server
        .post(
            &server.owner_token,
            "/v1/pairing/intents",
            &json!({"https_origin":"https://api.example"}),
        )
        .await
        .1["intent_token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(claim(&new).send().await.unwrap().status(), StatusCode::OK);
    server.shutdown().await;
}

#[tokio::test]
async fn pairing_intent_maintenance_removes_expired_and_consumed_rows_only() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let expired = [7_u8; 32];
    let active = [8_u8; 32];
    let consumed = [9_u8; 32];
    for digest in [expired, active, consumed] {
        sqlx::query("INSERT INTO pairing_intents(intent_digest,vault_id,origin,created_by_device_id,expires_at) VALUES($1,$2,'https://api.example',$3,now()+interval '1 hour')")
            .bind(digest.as_slice()).bind(server.vault).bind(server.owner_device).execute(&server.pool).await.unwrap();
    }
    sqlx::query(
        "UPDATE pairing_intents SET expires_at=now()-interval '1 second' WHERE intent_digest=$1",
    )
    .bind(expired.as_slice())
    .execute(&server.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE pairing_intents SET approved_at=now()-interval '10 minutes' WHERE intent_digest=$1",
    )
    .bind(consumed.as_slice())
    .execute(&server.pool)
    .await
    .unwrap();
    assert_eq!(
        prune_expired_pairing_intents(&server.pool).await.unwrap(),
        2
    );
    let survivors: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT intent_digest FROM pairing_intents ORDER BY intent_digest")
            .fetch_all(&server.pool)
            .await
            .unwrap();
    assert_eq!(survivors, vec![active.to_vec()]);
    server.shutdown().await;
}

#[tokio::test]
async fn device_revocation_enforces_owner_scope_last_owner_and_token_cleanup() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let device = server.pair("device").await;

    let (status, denied) = server
        .post(
            &device.token,
            &format!("/v1/devices/{}/revoke", server.owner_device),
            &Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["code"], "owner_required");

    let (status, missing) = server
        .post(
            &server.owner_token,
            &format!("/v1/devices/{}/revoke", Uuid::new_v4()),
            &Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["code"], "device_not_found");

    let (status, last_owner) = server
        .post(
            &server.owner_token,
            &format!("/v1/devices/{}/revoke", server.owner_device),
            &Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(last_owner["code"], "last_active_owner");

    let (status, body) = server
        .post(
            &server.owner_token,
            &format!("/v1/devices/{}/revoke", device.id),
            &Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, replayed) = server
        .post(
            &server.owner_token,
            &format!("/v1/devices/{}/revoke", device.id),
            &Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(replayed["code"], "device_already_revoked");
    let (status, rejected) = server.get(&device.token, "/v1/vault").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(rejected["code"], "invalid_bearer");
    server.shutdown().await;
}

#[tokio::test]
async fn vault_delete_is_owner_only_requires_echo_and_queues_all_object_cleanup() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let device = server.pair("device").await;
    let client = Client::new();

    let (status, denied) = server
        .call(
            client
                .delete(format!("{}/v1/vault", server.base_url))
                .bearer_auth(&device.token)
                .json(&json!({"vault_id":server.vault})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["code"], "owner_required");
    let (status, mismatch) = server
        .call(
            client
                .delete(format!("{}/v1/vault", server.base_url))
                .bearer_auth(&server.owner_token)
                .json(&json!({"vault_id":Uuid::new_v4()})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(mismatch["code"], "vault_id_mismatch");

    let reservation = Uuid::new_v4();
    let attachment = Uuid::new_v4();
    sqlx::query("INSERT INTO upload_reservations(vault_id,attachment_id,device_id,object_key,declared_bytes,declared_sha256) VALUES($1,$2,$3,'reserved-key',1,$4)")
        .bind(server.vault).bind(reservation).bind(server.owner_device).bind(vec![1_u8; 32]).execute(&server.pool).await.unwrap();
    sqlx::query("INSERT INTO attachments(vault_id,attachment_id,object_key,ciphertext_bytes,ciphertext_sha256,created_by_device_id) VALUES($1,$2,'private-key',1,$3,$4)")
        .bind(server.vault).bind(attachment).bind(vec![2_u8; 32]).bind(server.owner_device).execute(&server.pool).await.unwrap();
    sqlx::query("INSERT INTO storage_deletions(object_key,vault_id,reason,not_before) VALUES('private-key',$1,'upload_attempt',now()+interval '1 hour')")
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO public_attachment_copies(share_id,vault_id,attachment_id,object_key,token_digest,safe_name,media_type,byte_count,created_by_device_id,ready_at) VALUES($1,$2,$3,'public-key',$4,'file.bin','application/octet-stream',1,$5,now())")
        .bind(Uuid::new_v4()).bind(server.vault).bind(attachment).bind(vec![3_u8; 32]).bind(server.owner_device).execute(&server.pool).await.unwrap();
    server.commit(&server.owner_event(1)).await;

    let (status, deleted) = server
        .call(
            client
                .delete(format!("{}/v1/vault", server.base_url))
                .bearer_auth(&server.owner_token)
                .json(&json!({"vault_id":server.vault})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{deleted}");
    let vault_count: i64 = sqlx::query_scalar("SELECT count(*) FROM vaults WHERE vault_id=$1")
        .bind(server.vault)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(vault_count, 0);
    let remaining_dependents: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM devices WHERE vault_id=$1) + (SELECT count(*) FROM encrypted_records WHERE vault_id=$1) + (SELECT count(*) FROM public_attachment_copies WHERE vault_id=$1)")
        .bind(server.vault).fetch_one(&server.pool).await.unwrap();
    assert_eq!(remaining_dependents, 0);
    let queued: Vec<(String, String)> = sqlx::query_as(
        "SELECT object_key,reason FROM storage_deletions WHERE vault_id=$1 ORDER BY object_key",
    )
    .bind(server.vault)
    .fetch_all(&server.pool)
    .await
    .unwrap();
    assert_eq!(
        queued,
        vec![
            ("private-key".into(), "vault_deleted".into()),
            ("public-key".into(), "vault_deleted".into()),
            ("reserved-key".into(), "vault_deleted".into()),
        ]
    );
    let private_cleanup_still_deferred: bool = sqlx::query_scalar(
        "SELECT not_before > now()+interval '50 minutes' FROM storage_deletions WHERE object_key='private-key'",
    )
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert!(private_cleanup_still_deferred);
    server.shutdown().await;
}

#[tokio::test]
async fn cross_vault_foreign_gateway_and_non_target_receipts_are_denied() {
    let _guard = TEST_LOCK.lock().await;
    let first = TestServer::start().await;
    let target = first.pair("gateway").await;
    let other = first.pair("gateway").await;
    let foreign_vault = Uuid::new_v4();
    let (foreign_profile, foreign_fingerprint) = profile(foreign_vault, 1);
    let foreign_owner = create_owner(
        &first.pool,
        foreign_profile,
        vec![1, 2, 3],
        foreign_fingerprint.clone(),
        1,
    )
    .await
    .unwrap();
    let foreign = first
        .pair_for(
            &foreign_owner.device_token,
            foreign_vault,
            &foreign_fingerprint,
            "gateway",
        )
        .await;
    let client = Client::new();

    let foreign_envelope = set_fingerprint(
        event(first.vault, foreign_owner.device_id, 1, Uuid::new_v4(), 1),
        &first.fingerprint,
    );
    let denied = client
        .post(format!("{}/v1/events", first.base_url))
        .bearer_auth(&foreign_owner.device_token)
        .json(&foreign_envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let foreign_route = set_fingerprint(
        command(
            first.vault,
            first.owner_device,
            1,
            Uuid::new_v4(),
            Uuid::new_v4(),
            foreign.id,
        ),
        &first.fingerprint,
    );
    let denied = first
        .auth(client.post(format!("{}/v1/commands", first.base_url)))
        .json(&foreign_route)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let command_id = Uuid::new_v4();
    let valid = set_fingerprint(
        command(
            first.vault,
            first.owner_device,
            1,
            Uuid::new_v4(),
            command_id,
            target.id,
        ),
        &first.fingerprint,
    );
    let accepted = first
        .auth(client.post(format!("{}/v1/commands", first.base_url)))
        .json(&valid)
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let receipt_url = format!("{}/v1/commands/{command_id}/receipts", first.base_url);
    let denied = client
        .post(&receipt_url)
        .bearer_auth(&other.token)
        .json(&json!({"receipt":{"state":"sent"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let owner_denied = first
        .auth(client.post(&receipt_url))
        .json(&json!({"receipt":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(owner_denied.status(), StatusCode::FORBIDDEN);
    let accepted = client
        .post(&receipt_url)
        .bearer_auth(&target.token)
        .json(&json!({"receipt":{"state":"sent"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::NO_CONTENT);

    first.shutdown().await;
}

#[tokio::test]
async fn exact_retry_returns_cursor_and_changed_identity_or_sequence_conflicts() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let client = Client::new();
    let envelope_id = Uuid::new_v4();
    let original = set_fingerprint(
        event(server.vault, server.owner_device, 1, envelope_id, 1),
        &server.fingerprint,
    );
    let first: Value = server
        .auth(client.post(format!("{}/v1/events", server.base_url)))
        .json(&original)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, json!({"cursor":"1","duplicate":false}));
    let retry: Value = server
        .auth(client.post(format!("{}/v1/events", server.base_url)))
        .json(&original)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(retry, json!({"cursor":"1","duplicate":true}));

    let changed_envelope = set_fingerprint(
        event(server.vault, server.owner_device, 2, envelope_id, 2),
        &server.fingerprint,
    );
    let response = server
        .auth(client.post(format!("{}/v1/events", server.base_url)))
        .json(&changed_envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let changed_sequence = set_fingerprint(
        event(server.vault, server.owner_device, 1, Uuid::new_v4(), 3),
        &server.fingerprint,
    );
    let response = server
        .auth(client.post(format!("{}/v1/events", server.base_url)))
        .json(&changed_sequence)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    server.shutdown().await;
}

#[tokio::test]
async fn concurrent_commits_have_gapless_high_water_and_ordered_replay() {
    let _guard = TEST_LOCK.lock().await;
    let server = Arc::new(TestServer::start().await);
    let mut tasks = Vec::new();
    for sequence in 1..=16_u64 {
        let server = Arc::clone(&server);
        tasks.push(tokio::spawn(async move {
            let body = set_fingerprint(
                event(
                    server.vault,
                    server.owner_device,
                    sequence,
                    Uuid::new_v4(),
                    sequence as u8,
                ),
                &server.fingerprint,
            );
            let response = server
                .auth(Client::new().post(format!("{}/v1/events", server.base_url)))
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            (status, body)
        }));
    }
    for task in tasks {
        let (status, body) = task.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let replay: Value = server
        .auth(Client::new().get(format!("{}/v1/events?after=0&limit=100", server.base_url)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay["high_water_cursor"], "16");
    let cursors: Vec<u64> = replay["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["cursor"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(cursors, (1..=16).collect::<Vec<_>>());
    Arc::try_unwrap(server).ok().unwrap().shutdown().await;
}

#[tokio::test]
async fn websocket_handshake_replays_and_revocation_closes_active_socket() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let device = server.pair("device").await;
    let body = set_fingerprint(
        event(server.vault, server.owner_device, 1, Uuid::new_v4(), 4),
        &server.fingerprint,
    );
    assert_eq!(
        server
            .auth(Client::new().post(format!("{}/v1/events", server.base_url)))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let mut ws = server.connect_ws(&device.token).await;
    send_hello(&mut ws, "0").await;
    let ready = next_json(&mut ws, 2).await;
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["protocol_version"], 1);
    assert_eq!(ready["resume_cursor"], "0");
    assert_eq!(ready["high_water_cursor"], "1");
    let frame = next_json(&mut ws, 2).await;
    assert_eq!(frame["type"], "event");
    assert_eq!(frame["cursor"], "1");

    // This committed row deliberately bypasses the API broadcast channel.  A
    // connected peer must still receive it from its bounded durable scan.
    sqlx::query("INSERT INTO event_log(vault_id,cursor,envelope_id,producer_device_id,producer_sequence,purpose,cipher_digest,envelope) VALUES($1,2,$2,$3,2,'event',$4,$5)")
        .bind(server.vault)
        .bind(Uuid::new_v4())
        .bind(server.owner_device)
        .bind(vec![9_u8; 32])
        .bind(json!({"protocol_version":1}))
        .execute(&server.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE vaults SET next_cursor=2 WHERE vault_id=$1")
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    let replayed = tokio::time::timeout(Duration::from_secs(7), ws.next())
        .await
        .expect("durable replay timer did not deliver a no-notice commit")
        .unwrap()
        .unwrap();
    let replayed: Value = serde_json::from_str(replayed.to_text().unwrap()).unwrap();
    assert_eq!(replayed["cursor"], "2");

    let revoked = server
        .auth(Client::new().post(format!(
            "{}/v1/devices/{}/revoke",
            server.base_url, device.id
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        close_code(&mut ws, 22).await,
        4401,
        "revoked active websocket must close with the revoked code"
    );
    server.shutdown().await;
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl TestServer {
    async fn connect_ws(&self, token: &str) -> Ws {
        let request = tungstenite::http::Request::builder()
            .uri(&self.ws_url)
            .header(
                "Host",
                Url::parse(&self.ws_url).unwrap().host_str().unwrap(),
            )
            .header("Authorization", format!("Bearer {token}"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .unwrap();
        connect_async(request).await.unwrap().0
    }

    /// Sends a request and returns the status plus JSON body (`null` when empty).
    async fn call(&self, request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON body: {text}"))
        };
        (status, body)
    }

    async fn get(&self, token: &str, path: &str) -> (StatusCode, Value) {
        self.call(
            Client::new()
                .get(format!("{}{path}", self.base_url))
                .bearer_auth(token),
        )
        .await
    }

    async fn post(&self, token: &str, path: &str, body: &Value) -> (StatusCode, Value) {
        self.call(
            Client::new()
                .post(format!("{}{path}", self.base_url))
                .bearer_auth(token)
                .json(body),
        )
        .await
    }

    /// A valid owner-produced event at the vault's initial epoch.
    fn owner_event(&self, sequence: u64) -> Value {
        set_fingerprint(
            event(
                self.vault,
                self.owner_device,
                sequence,
                Uuid::new_v4(),
                sequence as u8,
            ),
            &self.fingerprint,
        )
    }

    async fn commit(&self, body: &Value) -> String {
        let (status, accepted) = self.post(&self.owner_token, "/v1/events", body).await;
        assert_eq!(status, StatusCode::OK, "{accepted}");
        accepted["cursor"].as_str().unwrap().to_owned()
    }
}

async fn send_hello(ws: &mut Ws, resume_cursor: &str) {
    ws.send(tungstenite::Message::Text(
        json!({"protocol_version":1,"resume_cursor":resume_cursor})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

/// Next text frame as JSON, skipping control frames.
async fn next_json(ws: &mut Ws, seconds: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(seconds), async {
        loop {
            match ws.next().await {
                Some(Ok(tungstenite::Message::Text(text))) => {
                    return serde_json::from_str::<Value>(&text).unwrap();
                }
                Some(Ok(tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_))) => {}
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for a websocket frame")
}

/// Waits for the server close frame and returns its code. Text frames that
/// precede it (for example `resync_required`) are skipped.
async fn close_code(ws: &mut Ws, seconds: u64) -> u16 {
    tokio::time::timeout(Duration::from_secs(seconds), async {
        loop {
            match ws.next().await {
                Some(Ok(tungstenite::Message::Close(Some(frame)))) => return u16::from(frame.code),
                Some(Ok(tungstenite::Message::Close(None))) => return 1005,
                Some(Ok(_)) => {}
                other => panic!("socket ended without a close frame: {other:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for a websocket close")
}

fn cursors(items: &Value) -> Vec<u64> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["cursor"].as_str().unwrap().parse().unwrap())
        .collect()
}

async fn wait_until<F, Fut>(seconds: u64, what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    while !condition().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn snapshot_pages_are_stable_under_concurrent_writes_and_converge_with_tail() {
    let _guard = TEST_LOCK.lock().await;
    let server = Arc::new(TestServer::start().await);
    for sequence in 1..=25 {
        server.commit(&server.owner_event(sequence)).await;
    }
    let (status, start) = server.get(&server.owner_token, "/v1/snapshot").await;
    assert_eq!(status, StatusCode::OK, "{start}");
    assert_eq!(start["high_water_cursor"], "25");
    assert_eq!(start["record_count"], "25");
    assert_eq!(start["vault_id"], json!(server.vault));

    let mut writers = Vec::new();
    for sequence in 26..=45_u64 {
        let server = Arc::clone(&server);
        writers.push(tokio::spawn(async move {
            server.commit(&server.owner_event(sequence)).await
        }));
    }
    let mut snapshot = Vec::new();
    let mut after = "0".to_owned();
    loop {
        let (status, page) = server
            .get(
                &server.owner_token,
                &format!("/v1/snapshot/records?high_water=25&after={after}&limit=7"),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["high_water_cursor"], "25");
        for record in page["records"].as_array().unwrap() {
            assert_eq!(record["envelope"]["vault_id"], json!(server.vault));
        }
        snapshot.extend(cursors(&page["records"]));
        match page["next_after"].as_str() {
            Some(next) => after = next.to_owned(),
            None => break,
        }
    }
    assert_eq!(snapshot, (1..=25).collect::<Vec<_>>());
    for writer in writers {
        writer.await.unwrap();
    }
    let (_, again) = server
        .get(
            &server.owner_token,
            "/v1/snapshot/records?high_water=25&after=7&limit=7",
        )
        .await;
    assert_eq!(cursors(&again["records"]), (8..=14).collect::<Vec<_>>());

    let mut tail = Vec::new();
    let mut after = 25_u64;
    loop {
        let (status, page) = server
            .get(
                &server.owner_token,
                &format!("/v1/events?after={after}&limit=200"),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let batch = cursors(&page["events"]);
        let Some(last) = batch.last().copied() else {
            break;
        };
        tail.extend(batch);
        after = last;
    }
    snapshot.extend(tail);
    assert_eq!(
        snapshot,
        (1..=45).collect::<Vec<_>>(),
        "cut plus tail must converge without gaps or duplicates"
    );

    let (status, ahead) = server
        .get(&server.owner_token, "/v1/snapshot/records?high_water=46")
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(ahead["code"], "resync_required");
    assert_eq!(ahead["reason"], "cursor_ahead");
    assert_eq!(ahead["high_water_cursor"], "45");
    let (status, _) = server
        .get(
            &server.owner_token,
            "/v1/snapshot/records?high_water=5&after=6",
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let foreign_vault = Uuid::new_v4();
    let (foreign_profile, foreign_fingerprint) = profile(foreign_vault, 1);
    let foreign = create_owner(
        &server.pool,
        foreign_profile,
        vec![1],
        foreign_fingerprint,
        1,
    )
    .await
    .unwrap();
    let (_, foreign_start) = server.get(&foreign.device_token, "/v1/snapshot").await;
    assert_eq!(foreign_start["record_count"], "0");
    let (_, foreign_page) = server
        .get(&foreign.device_token, "/v1/snapshot/records?high_water=0")
        .await;
    assert_eq!(foreign_page["records"], json!([]));
    Arc::try_unwrap(server).ok().unwrap().shutdown().await;
}

#[tokio::test]
async fn compaction_only_prunes_replay_expired_explicit_targets_and_fences_snapshot_pages() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let first = set_fingerprint(
        compacting_event(server.vault, server.owner_device, 1, Uuid::new_v4(), vec![]),
        &server.fingerprint,
    );
    let second = set_fingerprint(
        compacting_event(
            server.vault,
            server.owner_device,
            2,
            Uuid::new_v4(),
            vec![(server.owner_device, 1)],
        ),
        &server.fingerprint,
    );
    server.commit(&first).await;
    server.commit(&second).await;
    // A producer cannot reference its own current or future sequence.
    let forward = set_fingerprint(
        compacting_event(
            server.vault,
            server.owner_device,
            3,
            Uuid::new_v4(),
            vec![(server.owner_device, 4)],
        ),
        &server.fingerprint,
    );
    let (status, body) = server
        .post(&server.owner_token, "/v1/events", &forward)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // A cross-producer cycle has no retained representative and must remain intact.
    let gateway = server.pair("gateway").await;
    let third = set_fingerprint(
        compacting_event(
            server.vault,
            server.owner_device,
            3,
            Uuid::new_v4(),
            vec![(gateway.id, 1)],
        ),
        &server.fingerprint,
    );
    let fourth = set_fingerprint(
        compacting_event(
            server.vault,
            gateway.id,
            1,
            Uuid::new_v4(),
            vec![(server.owner_device, 3)],
        ),
        &server.fingerprint,
    );
    server.commit(&third).await;
    let (status, body) = server.post(&gateway.token, "/v1/events", &fourth).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = server
        .post(&gateway.token, "/v1/compaction/capability", &json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    sqlx::query("UPDATE event_log SET created_at=now()-interval '31 days' WHERE vault_id=$1")
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    assert_eq!(
        prune_replay_log(&server.pool, Duration::from_secs(30 * 86_400))
            .await
            .unwrap(),
        4
    );
    // No device has declared generation fencing yet: nothing may be deleted.
    assert_eq!(compact_records(&server.pool).await.unwrap(), 0);
    let (status, _) = server
        .post(&server.owner_token, "/v1/compaction/capability", &json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(compact_records(&server.pool).await.unwrap(), 1);
    let remaining: Vec<i64> = sqlx::query_scalar(
        "SELECT cursor FROM encrypted_records WHERE vault_id=$1 ORDER BY cursor",
    )
    .bind(server.vault)
    .fetch_all(&server.pool)
    .await
    .unwrap();
    assert_eq!(remaining, vec![2, 3, 4]);
    let (_, start) = server.get(&server.owner_token, "/v1/snapshot").await;
    assert_eq!(start["compaction_generation"], "1");
    let (status, restart) = server
        .get(
            &server.owner_token,
            "/v1/snapshot/records?high_water=4&after=0",
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(restart["code"], "resync_required");
    assert_eq!(restart["reason"], "compaction_generation_changed");
    let (status, page) = server
        .get(
            &server.owner_token,
            "/v1/snapshot/records?high_water=4&compaction_generation=1&after=0",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(cursors(&page["records"]), vec![2, 3, 4]);
    server.shutdown().await;
}

fn marked_event(
    vault: Uuid,
    producer: Uuid,
    sequence: u64,
    payload: u8,
    terminal: bool,
    supersedes: Vec<(Uuid, u64)>,
    fingerprint: &str,
) -> Value {
    let mut value = compacting_event(vault, producer, sequence, Uuid::new_v4(), supersedes);
    value["ciphertext"] = json!(base64::engine::general_purpose::STANDARD.encode([payload]));
    value["compaction"]["terminal"] = json!(terminal);
    set_fingerprint(value, fingerprint)
}

async fn age_and_prune(server: &TestServer) {
    sqlx::query("UPDATE event_log SET created_at=now()-interval '31 days' WHERE vault_id=$1")
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    prune_replay_log(&server.pool, Duration::from_secs(30 * 86_400))
        .await
        .unwrap();
    // Each vault is processed at most daily; tests rewind that clock explicitly.
    sqlx::query("UPDATE vaults SET last_compacted_at=NULL WHERE vault_id=$1")
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
}

async fn retained(server: &TestServer) -> Vec<i64> {
    sqlx::query_scalar("SELECT cursor FROM encrypted_records WHERE vault_id=$1 ORDER BY cursor")
        .bind(server.vault)
        .fetch_all(&server.pool)
        .await
        .unwrap()
}

fn keyed(mut value: Value, group: u8) -> Value {
    value["compaction"]["key"] =
        json!(base64::engine::general_purpose::STANDARD.encode([group; 32]));
    value
}

#[tokio::test]
async fn expired_terminal_root_is_kept_while_a_same_key_record_survives_outside_its_ancestry() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let (o, v, f) = (
        server.owner_device,
        server.vault,
        server.fingerprint.clone(),
    );
    // c1 is an older same-group state that the terminal never referenced (a strand).
    server
        .commit(&keyed(marked_event(v, o, 1, 1, false, vec![], &f), 9))
        .await;
    server
        .commit(&keyed(marked_event(v, o, 2, 2, false, vec![], &f), 9))
        .await;
    server
        .commit(&keyed(marked_event(v, o, 3, 3, true, vec![(o, 2)], &f), 9))
        .await;
    let (status, _) = server
        .post(&server.owner_token, "/v1/compaction/capability", &json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    age_and_prune(&server).await;
    sqlx::query(
        "UPDATE record_compaction SET created_at=now()-interval '91 days' WHERE vault_id=$1",
    )
    .bind(v)
    .execute(&server.pool)
    .await
    .unwrap();
    // c2 goes (superseded by c3); the terminal root stays because c1 survives.
    assert_eq!(compact_records(&server.pool).await.unwrap(), 1);
    assert_eq!(retained(&server).await, vec![1, 3]);
    server.shutdown().await;
}

#[tokio::test]
async fn expired_terminal_roots_self_expire_with_their_ancestry_and_keep_retry_identity() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let (o, v, f) = (
        server.owner_device,
        server.vault,
        server.fingerprint.clone(),
    );
    // Lifetime A: post (c1) -> removal (c2, terminal). Lifetime B: post (c3) -> removal (c4).
    server
        .commit(&keyed(marked_event(v, o, 1, 1, false, vec![], &f), 1))
        .await;
    server
        .commit(&keyed(marked_event(v, o, 2, 2, true, vec![(o, 1)], &f), 1))
        .await;
    server
        .commit(&keyed(marked_event(v, o, 3, 3, false, vec![], &f), 2))
        .await;
    server
        .commit(&keyed(marked_event(v, o, 4, 4, true, vec![(o, 3)], &f), 2))
        .await;
    // A standalone terminal (e.g. a dismissal) with no ancestry.
    server
        .commit(&keyed(marked_event(v, o, 5, 5, true, vec![], &f), 3))
        .await;
    // A non-terminal latest state never expires, however old.
    server
        .commit(&keyed(marked_event(v, o, 6, 6, false, vec![], &f), 4))
        .await;
    let (status, _) = server
        .post(&server.owner_token, "/v1/compaction/capability", &json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    age_and_prune(&server).await;

    // Young terminals stay, but the posts they supersede go.
    assert_eq!(compact_records(&server.pool).await.unwrap(), 2);
    assert_eq!(retained(&server).await, vec![2, 4, 5, 6]);

    // 90 days later for lifetime A and the standalone terminal only.
    sqlx::query("UPDATE record_compaction SET created_at=now()-interval '91 days' WHERE vault_id=$1 AND cursor IN (2,5,6)")
        .bind(v).execute(&server.pool).await.unwrap();
    sqlx::query("UPDATE vaults SET last_compacted_at=NULL WHERE vault_id=$1")
        .bind(v)
        .execute(&server.pool)
        .await
        .unwrap();
    assert_eq!(compact_records(&server.pool).await.unwrap(), 2);
    assert_eq!(
        retained(&server).await,
        vec![4, 6],
        "younger root and non-terminal state retained"
    );
    let (_, start) = server.get(&server.owner_token, "/v1/snapshot").await;
    assert_eq!(start["compaction_generation"], "2");

    // A retry of the purged terminal stays a duplicate of its original cursor.
    let ledger: Uuid = sqlx::query_scalar("SELECT envelope_id FROM compacted_records WHERE vault_id=$1 AND producer_device_id=$2 AND producer_sequence=2")
        .bind(v).bind(o).fetch_one(&server.pool).await.unwrap();
    let mut retry = keyed(marked_event(v, o, 2, 2, true, vec![(o, 1)], &f), 1);
    retry["envelope_id"] = json!(ledger);
    let (status, accepted) = server.post(&server.owner_token, "/v1/events", &retry).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["duplicate"], true);
    assert_eq!(accepted["cursor"], "2");

    // Lifetime B expires later; only the latest non-terminal state remains.
    sqlx::query("UPDATE record_compaction SET created_at=now()-interval '91 days' WHERE vault_id=$1 AND cursor=4")
        .bind(v).execute(&server.pool).await.unwrap();
    sqlx::query("UPDATE vaults SET last_compacted_at=NULL WHERE vault_id=$1")
        .bind(v)
        .execute(&server.pool)
        .await
        .unwrap();
    assert_eq!(compact_records(&server.pool).await.unwrap(), 1);
    assert_eq!(retained(&server).await, vec![6]);
    let remaining_meta: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM record_supersessions WHERE vault_id=$1")
            .bind(v)
            .fetch_one(&server.pool)
            .await
            .unwrap();
    assert_eq!(remaining_meta, 0);
    server.shutdown().await;
}

#[tokio::test]
async fn unsaturated_compaction_pass_waits_the_full_daily_interval() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let (o, v, f) = (
        server.owner_device,
        server.vault,
        server.fingerprint.clone(),
    );
    server
        .commit(&marked_event(v, o, 1, 1, false, vec![], &f))
        .await;
    server
        .commit(&marked_event(v, o, 2, 2, false, vec![(o, 1)], &f))
        .await;
    let (status, _) = server
        .post(&server.owner_token, "/v1/compaction/capability", &json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    age_and_prune(&server).await;
    assert_eq!(compact_records(&server.pool).await.unwrap(), 1);
    // An unsaturated pass waits the full interval.
    let next_due_hours: f64 = sqlx::query_scalar("SELECT (EXTRACT(EPOCH FROM (last_compacted_at + interval '24 hours' - now()))/3600)::float8 FROM vaults WHERE vault_id=$1")
        .bind(v).fetch_one(&server.pool).await.unwrap();
    assert!(next_due_hours > 23.0, "{next_due_hours}");
    server.shutdown().await;
}

#[tokio::test]
async fn compaction_collapses_chains_protects_targets_holds_pending_and_keeps_retry_identity() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let gateway = server.pair("gateway").await;
    let (o, v, f) = (
        server.owner_device,
        server.vault,
        server.fingerprint.clone(),
    );
    // 1 <- 2 <- 3(terminal): the whole superseded chain goes, not just its middle.
    server
        .commit(&marked_event(v, o, 1, 1, false, vec![], &f))
        .await; // c1
    server
        .commit(&marked_event(v, o, 2, 2, false, vec![(o, 1)], &f))
        .await; // c2
    server
        .commit(&marked_event(v, o, 3, 3, true, vec![(o, 2)], &f))
        .await; // c3
    // A gateway legacy row (no metadata) and a carrier command are not removable by the owner.
    let (status, _) = server
        .post(
            &gateway.token,
            "/v1/events",
            &set_fingerprint(event(v, gateway.id, 1, Uuid::new_v4(), 40), &f),
        )
        .await; // c4
    assert_eq!(status, StatusCode::OK);
    let (status, body) = server
        .post(
            &server.owner_token,
            "/v1/commands",
            &set_fingerprint(
                command(v, o, 4, Uuid::new_v4(), Uuid::new_v4(), gateway.id),
                &f,
            ),
        )
        .await; // c5
    assert_eq!(status, StatusCode::OK, "{body}");
    server
        .commit(&marked_event(
            v,
            o,
            5,
            5,
            false,
            vec![(gateway.id, 1), (o, 4)],
            &f,
        ))
        .await; // c6
    // Young terminal 7 superseded by 8: kept for the terminal window.
    server
        .commit(&marked_event(v, o, 7, 7, true, vec![], &f))
        .await; // c7
    server
        .commit(&marked_event(v, o, 8, 8, false, vec![(o, 7)], &f))
        .await; // c8
    // 9 <- 10 <- 11, but 10 also names not-yet-uploaded (gateway,50): 10 is held, 9 still goes.
    server
        .commit(&marked_event(v, o, 9, 9, false, vec![], &f))
        .await; // c9
    server
        .commit(&marked_event(
            v,
            o,
            10,
            10,
            false,
            vec![(o, 9), (gateway.id, 50)],
            &f,
        ))
        .await; // c10
    server
        .commit(&marked_event(v, o, 11, 11, false, vec![(o, 10)], &f))
        .await; // c11
    age_and_prune(&server).await;

    // The gateway has not declared generation fencing yet.
    assert_eq!(compact_records(&server.pool).await.unwrap(), 0);
    let (_, start) = server.get(&server.owner_token, "/v1/snapshot").await;
    assert_eq!(start["compaction_supported"], true);
    assert_eq!(start["compaction_active"], false);
    let (status, _) = server
        .post(&server.owner_token, "/v1/compaction/capability", &json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // A fenced snapshot page is itself the gateway's declaration.
    let (status, _) = server
        .get(
            &gateway.token,
            "/v1/snapshot/records?high_water=11&compaction_generation=0&after=0",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, start) = server.get(&server.owner_token, "/v1/snapshot").await;
    assert_eq!(start["compaction_active"], true);

    assert_eq!(compact_records(&server.pool).await.unwrap(), 3);
    assert_eq!(retained(&server).await, vec![3, 4, 5, 6, 7, 8, 10, 11]);
    // Daily spacing: an immediate second pass does nothing even when eligible later.
    sqlx::query("UPDATE record_compaction SET created_at=now()-interval '91 days' WHERE vault_id=$1 AND cursor=7")
        .bind(v).execute(&server.pool).await.unwrap();
    assert_eq!(compact_records(&server.pool).await.unwrap(), 0);
    sqlx::query("UPDATE vaults SET last_compacted_at=NULL WHERE vault_id=$1")
        .bind(v)
        .execute(&server.pool)
        .await
        .unwrap();
    assert_eq!(
        compact_records(&server.pool).await.unwrap(),
        1,
        "expired terminal 7 is superseded by 8"
    );
    assert_eq!(retained(&server).await, vec![3, 4, 5, 6, 8, 10, 11]);
    let (_, start) = server.get(&server.owner_token, "/v1/snapshot").await;
    assert_eq!(start["compaction_generation"], "2");

    // Retry identity survives physical deletion.
    let original = marked_event(v, o, 1, 1, false, vec![], &f);
    let mut retry = original.clone();
    let ledger: Uuid = sqlx::query_scalar("SELECT envelope_id FROM compacted_records WHERE vault_id=$1 AND producer_sequence=1 AND producer_device_id=$2")
        .bind(v).bind(o).fetch_one(&server.pool).await.unwrap();
    retry["envelope_id"] = json!(ledger);
    let (status, accepted) = server.post(&server.owner_token, "/v1/events", &retry).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["duplicate"], true);
    assert_eq!(accepted["cursor"], "1");
    let reused = marked_event(v, o, 2, 99, false, vec![], &f);
    let (status, conflict) = server
        .post(&server.owner_token, "/v1/events", &reused)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(
        retained(&server).await,
        vec![3, 4, 5, 6, 8, 10, 11],
        "no record was re-added"
    );

    // A newly paired device pauses physical compaction until it declares.
    let late = server.pair("device").await;
    let (_, start) = server.get(&late.token, "/v1/snapshot").await;
    assert_eq!(start["compaction_active"], false);
    server.shutdown().await;
}

#[tokio::test]
async fn replay_expiry_and_ahead_cursors_resync_while_immutable_records_survive_pruning() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let mut bodies = Vec::new();
    for sequence in 1..=10 {
        let body = server.owner_event(sequence);
        server.commit(&body).await;
        bodies.push(body);
    }
    sqlx::query(
        "UPDATE event_log SET created_at=now()-interval '31 days' WHERE vault_id=$1 AND cursor<=6",
    )
    .bind(server.vault)
    .execute(&server.pool)
    .await
    .unwrap();
    let retention = Duration::from_secs(30 * 86_400);
    assert_eq!(prune_replay_log(&server.pool, retention).await.unwrap(), 6);
    assert_eq!(prune_replay_log(&server.pool, retention).await.unwrap(), 0);

    for expired in ["0", "5"] {
        let (status, body) = server
            .get(&server.owner_token, &format!("/v1/events?after={expired}"))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "resync_required");
        assert_eq!(body["reason"], "cursor_expired");
        assert_eq!(body["replay_floor_cursor"], "6");
        assert_eq!(body["high_water_cursor"], "10");
    }
    let (status, retained) = server.get(&server.owner_token, "/v1/events?after=6").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cursors(&retained["events"]), vec![7, 8, 9, 10]);
    assert_eq!(retained["replay_floor_cursor"], "6");
    let (status, ahead) = server.get(&server.owner_token, "/v1/events?after=11").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(ahead["reason"], "cursor_ahead");

    let (_, start) = server.get(&server.owner_token, "/v1/snapshot").await;
    assert_eq!(start["record_count"], "10");
    assert_eq!(start["replay_floor_cursor"], "6");
    let (_, page) = server
        .get(
            &server.owner_token,
            "/v1/snapshot/records?high_water=10&limit=200",
        )
        .await;
    assert_eq!(cursors(&page["records"]), (1..=10).collect::<Vec<_>>());

    // Identity outlives the pruned transport rows.
    let (status, retry) = server
        .post(&server.owner_token, "/v1/events", &bodies[0])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retry, json!({"cursor":"1","duplicate":true}));
    let (status, conflict) = server
        .post(&server.owner_token, "/v1/events", &server.owner_event(2))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "idempotency_conflict");

    assert!(
        sqlx::query("DELETE FROM encrypted_records WHERE vault_id=$1")
            .bind(server.vault)
            .execute(&server.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE encrypted_records SET envelope='{}'::jsonb WHERE vault_id=$1")
            .bind(server.vault)
            .execute(&server.pool)
            .await
            .is_err()
    );

    for (resume, reason) in [("0", "cursor_expired"), ("99", "cursor_ahead")] {
        let mut ws = server.connect_ws(&server.owner_token).await;
        send_hello(&mut ws, resume).await;
        let frame = next_json(&mut ws, 3).await;
        assert_eq!(frame["type"], "resync_required");
        assert_eq!(frame["reason"], reason);
        assert_eq!(close_code(&mut ws, 3).await, 4409);
    }
    server.shutdown().await;
}

#[tokio::test]
async fn websocket_negotiation_has_distinct_close_codes() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start_with(TransportOptions {
        hello_timeout: Duration::from_secs(1),
        ..TransportOptions::default()
    })
    .await;
    let token = server.owner_token.clone();

    let mut silent = server.connect_ws(&token).await;
    assert_eq!(close_code(&mut silent, 4).await, 4408);

    let mut old = server.connect_ws(&token).await;
    old.send(tungstenite::Message::Text(
        json!({"protocol_version":2,"resume_cursor":"0"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    assert_eq!(close_code(&mut old, 3).await, 4426);

    for invalid in [
        "not json",
        r#"{"protocol_version":1,"resume_cursor":"-1"}"#,
        r#"{"protocol_version":1}"#,
    ] {
        let mut ws = server.connect_ws(&token).await;
        ws.send(tungstenite::Message::Text(invalid.into()))
            .await
            .unwrap();
        assert_eq!(close_code(&mut ws, 3).await, 4400, "{invalid}");
    }

    let mut chatty = server.connect_ws(&token).await;
    send_hello(&mut chatty, "0").await;
    assert_eq!(next_json(&mut chatty, 3).await["type"], "ready");
    chatty
        .send(tungstenite::Message::Text("unexpected".into()))
        .await
        .unwrap();
    assert_eq!(close_code(&mut chatty, 3).await, 4400);
    server.shutdown().await;
}

#[tokio::test]
async fn outbox_drainer_recovers_a_lost_hint_and_retires_references() {
    let _guard = TEST_LOCK.lock().await;
    // The durable rescan is pushed beyond the test window, so only the
    // drainer's hint can deliver the injected commit in time.
    let server = TestServer::start_with(TransportOptions {
        durable_replay_interval: Duration::from_secs(120),
        outbox_drain_interval: Duration::from_millis(100),
        outbox_delivered_retention: Duration::from_secs(1),
        ..TransportOptions::default()
    })
    .await;
    let device = server.pair("device").await;
    let mut ws = server.connect_ws(&device.token).await;
    send_hello(&mut ws, "0").await;
    assert_eq!(next_json(&mut ws, 3).await["type"], "ready");

    assert_eq!(server.commit(&server.owner_event(1)).await, "1");
    assert_eq!(next_json(&mut ws, 3).await["cursor"], "1");
    let payload_is_null: bool = sqlx::query_scalar(
        "SELECT payload IS NULL FROM outbox_jobs WHERE vault_id=$1 AND cursor=1",
    )
    .bind(server.vault)
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert!(
        payload_is_null,
        "outbox rows are references, not envelope copies"
    );
    let outbox_state = |cursor: i64| {
        let pool = server.pool.clone();
        let vault = server.vault;
        async move {
            sqlx::query_as::<_, (bool, i32)>("SELECT delivered_at IS NOT NULL, attempts FROM outbox_jobs WHERE vault_id=$1 AND cursor=$2")
                .bind(vault)
                .bind(cursor)
                .fetch_optional(&pool)
                .await
                .unwrap()
        }
    };
    wait_until(5, "delivered outbox reference to be retired", || async {
        outbox_state(1).await.is_none()
    })
    .await;

    // Simulate a commit whose request-path hint was lost (crash or cancellation).
    sqlx::query("INSERT INTO event_log(vault_id,cursor,envelope_id,producer_device_id,producer_sequence,purpose,cipher_digest,envelope) VALUES($1,2,$2,$3,2,'event',$4,$5)")
        .bind(server.vault)
        .bind(Uuid::new_v4())
        .bind(server.owner_device)
        .bind(vec![9_u8; 32])
        .bind(json!({"protocol_version":1}))
        .execute(&server.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE vaults SET next_cursor=2 WHERE vault_id=$1")
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO outbox_jobs(vault_id,cursor,kind) VALUES($1,2,'sync')")
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    assert_eq!(next_json(&mut ws, 3).await["cursor"], "2");
    wait_until(3, "first delivery to be recorded", || async {
        outbox_state(2).await.is_none_or(|(delivered, _)| delivered)
    })
    .await;
    wait_until(5, "second reference to be retired", || async {
        outbox_state(2).await.is_none()
    })
    .await;

    // A redelivered reference is idempotent: the socket does not repeat cursor 2.
    sqlx::query("INSERT INTO outbox_jobs(vault_id,cursor,kind) VALUES($1,2,'sync')")
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    wait_until(3, "redelivery to be recorded", || async {
        outbox_state(2)
            .await
            .is_none_or(|(delivered, attempts)| delivered && attempts == 1)
    })
    .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(1_500), ws.next())
            .await
            .is_err(),
        "a repeated hint must not emit a duplicate frame"
    );
    server.shutdown().await;
}

fn with_epoch(mut envelope: Value, epoch: u32, fingerprint: &str) -> Value {
    envelope["key_epoch"] = json!(epoch);
    envelope["profile_fingerprint"] = json!(fingerprint);
    envelope
}

#[tokio::test]
async fn key_profiles_are_owner_only_immutable_forward_and_gate_epochs() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let owner = server.owner_token.clone();
    let gateway = server.pair("gateway").await;
    let device = server.pair("device").await;
    let (profile_two, fingerprint_two) = profile(server.vault, 2);
    let header_two = base64::engine::general_purpose::STANDARD.encode([2_u8; 80]);
    let register = json!({
        "key_epoch": 2,
        "public_key_profile": profile_two,
        "encrypted_vault_check_header": header_two,
        "profile_fingerprint": fingerprint_two,
    });

    let (status, _) = server
        .post(&device.token, "/v1/vault/key-profiles", &register)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, created) = server
        .post(&owner, "/v1/vault/key-profiles", &register)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["activated"], false);
    let (status, duplicate) = server
        .post(&owner, "/v1/vault/key-profiles", &register)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(duplicate["duplicate"], true);
    let mut changed = register.clone();
    changed["encrypted_vault_check_header"] = json!("AAAA");
    let (status, body) = server
        .post(&owner, "/v1/vault/key-profiles", &changed)
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("key_profile_conflict"))
    );
    let (foreign_profile, foreign_fingerprint) = profile(Uuid::new_v4(), 3);
    let (status, _) = server
        .post(&owner, "/v1/vault/key-profiles", &json!({"key_epoch":3,"public_key_profile":foreign_profile,"encrypted_vault_check_header":"AAAA","profile_fingerprint":foreign_fingerprint}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, vault) = server.get(&owner, "/v1/vault").await;
    assert_eq!(vault["key_epoch"], 1);

    let early = with_epoch(server.owner_event(1), 2, &fingerprint_two);
    let (status, body) = server.post(&owner, "/v1/events", &early).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("key_epoch_not_active"))
    );

    let accepted_command = set_fingerprint(
        command(
            server.vault,
            server.owner_device,
            2,
            Uuid::new_v4(),
            Uuid::new_v4(),
            gateway.id,
        ),
        &server.fingerprint,
    );
    let (status, original) = server.post(&owner, "/v1/commands", &accepted_command).await;
    assert_eq!(status, StatusCode::OK, "{original}");

    let activate = "/v1/vault/key-profiles/2/activate";
    let (status, _) = server.post(&device.token, activate, &json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, activated) = server.post(&owner, activate, &json!({})).await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    assert_eq!(activated["duplicate"], false);
    let (_, again) = server.post(&owner, activate, &json!({})).await;
    assert_eq!(again["duplicate"], true);
    let (status, body) = server
        .post(&owner, "/v1/vault/key-profiles/1/activate", &json!({}))
        .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("key_epoch_not_forward"))
    );
    let (status, _) = server
        .post(&owner, "/v1/vault/key-profiles/9/activate", &json!({}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, vault) = server.get(&owner, "/v1/vault").await;
    assert_eq!(vault["key_epoch"], 2);
    assert_eq!(vault["profile_fingerprint"], json!(fingerprint_two));
    assert_eq!(
        vault["encrypted_vault_check_header"],
        json!(header_two),
        "header must be unwrapped base64"
    );
    let (_, history) = server.get(&device.token, "/v1/vault/key-profiles").await;
    let history = history["key_profiles"].as_array().unwrap().clone();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["key_epoch"], 1);
    assert_eq!(history[0]["encrypted_vault_check_header"], "BwgJ");
    assert_eq!(history[0]["profile_fingerprint"], json!(server.fingerprint));
    assert_eq!(history[0]["current"], false);
    assert_eq!(history[1]["current"], true);

    let (status, retry) = server.post(&owner, "/v1/commands", &accepted_command).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retry["cursor"], original["cursor"]);
    assert_eq!(retry["duplicate"], true);
    let retired = set_fingerprint(
        command(
            server.vault,
            server.owner_device,
            3,
            Uuid::new_v4(),
            Uuid::new_v4(),
            gateway.id,
        ),
        &server.fingerprint,
    );
    let (status, body) = server.post(&owner, "/v1/commands", &retired).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("retired_key_epoch"))
    );
    let (status, _) = server
        .post(&owner, "/v1/events", &server.owner_event(4))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "historical activated epochs remain valid for events"
    );
    let (status, _) = server
        .post(
            &owner,
            "/v1/events",
            &with_epoch(server.owner_event(5), 2, &fingerprint_two),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let current_command = with_epoch(
        command(
            server.vault,
            server.owner_device,
            6,
            Uuid::new_v4(),
            Uuid::new_v4(),
            gateway.id,
        ),
        2,
        &fingerprint_two,
    );
    let (status, _) = server.post(&owner, "/v1/commands", &current_command).await;
    assert_eq!(status, StatusCode::OK);

    assert!(sqlx::query("UPDATE vault_key_profiles SET encrypted_vault_check_header='\\x00' WHERE vault_id=$1 AND key_epoch=1").bind(server.vault).execute(&server.pool).await.is_err());
    assert!(
        sqlx::query("DELETE FROM vault_key_profiles WHERE vault_id=$1")
            .bind(server.vault)
            .execute(&server.pool)
            .await
            .is_err()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn pending_command_references_never_retarget_a_gateway() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let owner = server.owner_token.clone();
    let first = server.pair("gateway").await;
    let second = server.pair("gateway").await;
    let envelope_id = Uuid::new_v4();
    let command_id = Uuid::new_v4();
    let original = set_fingerprint(
        command(
            server.vault,
            server.owner_device,
            1,
            envelope_id,
            command_id,
            first.id,
        ),
        &server.fingerprint,
    );
    let (status, accepted) = server.post(&owner, "/v1/commands", &original).await;
    assert_eq!(status, StatusCode::OK);
    let delivered_id = Uuid::new_v4();
    let delivered = set_fingerprint(
        command(
            server.vault,
            server.owner_device,
            2,
            Uuid::new_v4(),
            delivered_id,
            second.id,
        ),
        &server.fingerprint,
    );
    assert_eq!(
        server.post(&owner, "/v1/commands", &delivered).await.0,
        StatusCode::OK
    );
    let (status, _) = server
        .post(
            &second.token,
            &format!("/v1/commands/{delivered_id}/receipts"),
            &json!({"receipt":{"state":"sent"}}),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, pending) = server.get(&owner, "/v1/commands/pending").await;
    assert_eq!(
        pending["commands"],
        json!([{"command_id":command_id,"producer_device_id":server.owner_device,"gateway_device_id":first.id,"cursor":accepted["cursor"]}])
    );
    let (_, targeted) = server.get(&first.token, "/v1/commands/pending").await;
    assert_eq!(targeted["commands"].as_array().unwrap().len(), 1);
    let (_, other) = server.get(&second.token, "/v1/commands/pending").await;
    assert_eq!(other["commands"], json!([]));

    let mut retarget = original.clone();
    retarget["route"]["gateway_device_id"] = json!(second.id);
    let (status, _) = server.post(&owner, "/v1/commands", &retarget).await;
    assert_eq!(status, StatusCode::CONFLICT);
    retarget["envelope_id"] = json!(Uuid::new_v4());
    retarget["producer_sequence"] = json!("3");
    let (status, _) = server.post(&owner, "/v1/commands", &retarget).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a command id cannot move to another gateway"
    );

    let (status, _) = server
        .post(
            &owner,
            &format!("/v1/devices/{}/revoke", first.id),
            &json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, retry) = server.post(&owner, "/v1/commands", &original).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retry, json!({"cursor":accepted["cursor"],"duplicate":true}));
    let (_, pending) = server.get(&owner, "/v1/commands/pending").await;
    assert_eq!(pending["commands"][0]["gateway_device_id"], json!(first.id));
    let (status, _) = server
        .post(
            &second.token,
            &format!("/v1/commands/{command_id}/receipts"),
            &json!({"receipt":{"state":"sent"}}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, page) = server
        .get(
            &owner,
            &format!(
                "/v1/snapshot/records?high_water={}",
                accepted["cursor"].as_str().unwrap()
            ),
        )
        .await;
    assert_eq!(
        page["records"][0]["envelope"]["route"]["gateway_device_id"],
        json!(first.id)
    );
    server.shutdown().await;
}

#[tokio::test]
async fn legacy_local_schema_upgrades_without_losing_records_or_identity() {
    let _guard = TEST_LOCK.lock().await;
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL is required; integration tests never silently skip");
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("peppy_server_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated_url: Url = database_url.parse().unwrap();
    isolated_url
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(isolated_url.as_str())
        .await
        .unwrap();

    let full = sqlx::migrate!("./migrations");
    let mut legacy = sqlx::migrate!("./migrations");
    legacy.migrations = std::borrow::Cow::Owned(
        full.migrations
            .iter()
            .filter(|m| m.version < 7)
            .cloned()
            .collect(),
    );
    legacy.run(&pool).await.unwrap();

    // Rows exactly as the pre-A4 server wrote them.
    let vault = Uuid::new_v4();
    let owner_device = Uuid::new_v4();
    let gateway = Uuid::new_v4();
    let (vault_profile, fingerprint) = profile(vault, 1);
    let token = "ab".repeat(48);
    sqlx::query("INSERT INTO vaults(vault_id,public_key_profile,encrypted_vault_check_header,key_epoch,profile_fingerprint,next_cursor) VALUES($1,$2,$3,1,$4,2)")
        .bind(vault).bind(&vault_profile).bind(vec![7_u8, 8, 9]).bind(&fingerprint)
        .execute(&pool).await.unwrap();
    for (device, role) in [(owner_device, "owner"), (gateway, "gateway")] {
        sqlx::query("INSERT INTO devices(vault_id,device_id,role,public_key,profile_fingerprint,key_epoch) VALUES($1,$2,$3,'{}'::jsonb,$4,1)")
            .bind(vault).bind(device).bind(role).bind(&fingerprint)
            .execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO device_credentials(token_digest,vault_id,device_id) VALUES($1,$2,$3)")
        .bind(Sha256::digest(token.as_bytes()).as_slice())
        .bind(vault)
        .bind(owner_device)
        .execute(&pool)
        .await
        .unwrap();
    let command_id = Uuid::new_v4();
    let legacy_event = set_fingerprint(
        event(vault, owner_device, 1, Uuid::new_v4(), 1),
        &fingerprint,
    );
    let legacy_command = set_fingerprint(
        command(vault, owner_device, 2, Uuid::new_v4(), command_id, gateway),
        &fingerprint,
    );
    for (cursor, body) in [(1_i64, &legacy_event), (2, &legacy_command)] {
        let envelope: peppy_protocol::Envelope = serde_json::from_value(body.clone()).unwrap();
        let digest = envelope.wire_digest().unwrap();
        let canonical = serde_json::to_value(&envelope).unwrap();
        let purpose = if envelope.command_id.is_some() {
            "command"
        } else {
            "event"
        };
        sqlx::query("INSERT INTO event_log(vault_id,cursor,envelope_id,producer_device_id,producer_sequence,purpose,command_id,cipher_digest,envelope) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(vault).bind(cursor).bind(envelope.envelope_id.0).bind(owner_device).bind(cursor)
            .bind(purpose).bind(envelope.command_id.map(|id| id.0)).bind(digest.as_slice()).bind(&canonical)
            .execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO encrypted_records(vault_id,envelope_id,envelope) VALUES($1,$2,$3)",
        )
        .bind(vault)
        .bind(envelope.envelope_id.0)
        .bind(&canonical)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO outbox_jobs(vault_id,cursor,kind,payload) VALUES($1,$2,'sync',$3)",
        )
        .bind(vault)
        .bind(cursor)
        .bind(&canonical)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO commands(vault_id,producer_device_id,command_id,gateway_device_id,cipher_digest,cursor) SELECT vault_id,producer_device_id,command_id,$2,cipher_digest,cursor FROM event_log WHERE vault_id=$1 AND command_id IS NOT NULL")
        .bind(vault).bind(gateway).execute(&pool).await.unwrap();

    full.run(&pool).await.unwrap();

    let backfilled: Vec<(i64, Uuid, i64, String)> = sqlx::query_as("SELECT cursor,producer_device_id,producer_sequence,purpose FROM encrypted_records WHERE vault_id=$1 ORDER BY cursor")
        .bind(vault).fetch_all(&pool).await.unwrap();
    assert_eq!(
        backfilled,
        vec![
            (1, owner_device, 1, "event".into()),
            (2, owner_device, 2, "command".into())
        ]
    );
    let (epoch, activated): (i32, bool) = sqlx::query_as(
        "SELECT key_epoch, activated_at IS NOT NULL FROM vault_key_profiles WHERE vault_id=$1",
    )
    .bind(vault)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((epoch, activated), (1, true));
    let preserved: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM outbox_jobs WHERE vault_id=$1 AND payload IS NOT NULL",
    )
    .bind(vault)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        preserved, 2,
        "legacy outbox rows are not rewritten by the migration"
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server_pool = pool.clone();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            router_with_options(server_pool, TransportOptions::default()),
        )
        .await
        .unwrap();
    });
    let client = Client::new();
    let snapshot: Value = client
        .get(format!("{base_url}/v1/snapshot"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["record_count"], "2");
    assert_eq!(snapshot["high_water_cursor"], "2");
    let retry: Value = client
        .post(format!("{base_url}/v1/commands"))
        .bearer_auth(&token)
        .json(&legacy_command)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(retry, json!({"cursor":"2","duplicate":true}));
    let accepted: Value = client
        .post(format!("{base_url}/v1/events"))
        .bearer_auth(&token)
        .json(&set_fingerprint(
            event(vault, owner_device, 3, Uuid::new_v4(), 3),
            &fingerprint,
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(accepted, json!({"cursor":"3","duplicate":false}));

    task.abort();
    let _ = task.await;
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}

#[tokio::test]
async fn near_limit_envelopes_page_within_the_byte_budget_and_advance() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let budget = 8 * 1024 * 1024_usize;
    for sequence in 1..=8_u64 {
        let mut body = server.owner_event(sequence);
        body["ciphertext"] =
            json!(
                base64::engine::general_purpose::STANDARD.encode(vec![sequence as u8; 1_048_576])
            );
        assert_eq!(server.commit(&body).await, sequence.to_string());
    }

    for (path, items) in [
        ("/v1/events?limit=200&after=", "events"),
        (
            "/v1/snapshot/records?high_water=8&limit=200&after=",
            "records",
        ),
    ] {
        let mut seen: Vec<u64> = Vec::new();
        let mut after = "0".to_owned();
        let mut pages = 0;
        loop {
            let response = Client::new()
                .get(format!("{}{path}{after}", server.base_url))
                .bearer_auth(&server.owner_token)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = response.bytes().await.unwrap();
            // One envelope (~1.4 MB) may exceed the remaining budget, never more.
            assert!(
                bytes.len() < budget + 1_500_000,
                "{path} page was {} bytes",
                bytes.len()
            );
            let page: Value = serde_json::from_slice(&bytes).unwrap();
            let batch = cursors(&page[items]);
            assert!(
                !batch.is_empty() && batch.len() < 8,
                "{path} page must be budget-limited"
            );
            seen.extend(&batch);
            pages += 1;
            match page["next_after"].as_str() {
                Some(next) => {
                    assert_eq!(next, batch.last().unwrap().to_string());
                    after = next.to_owned();
                }
                None => break,
            }
        }
        assert_eq!(seen, (1..=8).collect::<Vec<_>>(), "{path}");
        assert!(pages >= 2, "{path}");
    }

    let mut ws = server.connect_ws(&server.owner_token).await;
    send_hello(&mut ws, "0").await;
    assert_eq!(next_json(&mut ws, 5).await["type"], "ready");
    for cursor in 1..=8 {
        let frame = next_json(&mut ws, 10).await;
        assert_eq!(frame["cursor"], cursor.to_string());
        assert_eq!(
            frame["envelope"]["ciphertext"].as_str().unwrap().len(),
            1_398_104
        );
    }
    server.shutdown().await;
}

/// One real path: a real client publishes legacy history, upgrades, removes; the real server
/// ages, prunes and compacts it; a fresh real client imports the fenced snapshot and nothing
/// resurrects.
#[tokio::test]
async fn real_client_history_purges_on_the_real_server_and_fresh_snapshot_does_not_resurrect() {
    use peppy_client_core as core;
    let _guard = TEST_LOCK.lock().await;
    let pass = "correct horse battery staple";
    let vault = Uuid::new_v4();
    let key_profile = peppy_crypto::KeyProfile::new(vault, 1).unwrap();
    let root = peppy_crypto::derive_root_key(pass, &key_profile).unwrap();
    let header = peppy_crypto::create_vault_check_header(&root, key_profile.clone()).unwrap();
    let server = TestServer::start_real(&key_profile, &header).await;
    let dir = tempfile::TempDir::new().unwrap();
    let open = |name: &str, device: Uuid| {
        let client = core::Client::open(
            core::ClientConfig {
                database_path: dir.path().join(name),
                vault_id: core::VaultId(vault),
                device_id: core::DeviceId(device),
            },
            core::DatabaseKey::new(&[5; 32]).unwrap(),
        )
        .unwrap();
        client.unlock(&key_profile, &header, pass).unwrap();
        client
    };
    let upload = |client: &core::Client| {
        let pending = client.pending_outbox().unwrap();
        let server = &server;
        async move {
            for envelope in &pending {
                let (status, body) = server
                    .post(
                        &server.owner_token,
                        "/v1/events",
                        &serde_json::to_value(envelope).unwrap(),
                    )
                    .await;
                assert_eq!(status, StatusCode::OK, "{body}");
            }
            pending
        }
    };
    let connect = |client: &core::Client, start: &Value| -> Value {
        serde_json::from_str(
            &client
                .set_server_compaction_state(
                    start["compaction_supported"].as_bool().unwrap(),
                    start["compaction_active"].as_bool().unwrap(),
                )
                .unwrap(),
        )
        .unwrap()
    };
    let phone = open("phone.db", server.owner_device);
    let capture = |text: &str| core::NotificationCapture {
        notification_key: "progress".into(),
        instance: "i".into(),
        package_name: "com.example".into(),
        app_name: "App".into(),
        title: "Download".into(),
        text: text.into(),
        category: None,
        posted_at: 1,
        dismissible: true,
    };
    // Pre-upgrade: three legacy posts (no server capability recorded yet).
    for text in ["10%", "50%", "90%"] {
        phone.capture_notification(capture(text)).unwrap();
    }
    let legacy = upload(&phone).await;
    assert_eq!(legacy.len(), 3);
    assert!(legacy.iter().all(|e| e.compaction.is_none()));
    for e in &legacy {
        phone.ack_outbox(e.envelope_id).unwrap();
    }

    // Upgrade handshake: declare, read the snapshot fields, record them.
    let (status, _) = server
        .post(&server.owner_token, "/v1/compaction/capability", &json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, start) = server.get(&server.owner_token, "/v1/snapshot").await;
    let ready = connect(&phone, &start);
    assert_eq!(ready["state"], "ready", "{ready}");
    assert_eq!(ready["history_compacting"], true);
    phone.remove_notification("progress", "i").unwrap();
    let removal = upload(&phone).await;
    assert_eq!(removal.len(), 1);
    assert_eq!(removal[0].compaction.as_ref().unwrap().supersedes.len(), 3);

    // Age everything past the replay window and the terminal window, then a real pass.
    sqlx::query("UPDATE event_log SET created_at=now()-interval '31 days' WHERE vault_id=$1")
        .bind(vault)
        .execute(&server.pool)
        .await
        .unwrap();
    prune_replay_log(&server.pool, Duration::from_secs(30 * 86_400))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE record_compaction SET created_at=now()-interval '91 days' WHERE vault_id=$1",
    )
    .bind(vault)
    .execute(&server.pool)
    .await
    .unwrap();
    assert_eq!(compact_records(&server.pool).await.unwrap(), 4);
    let retained: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM encrypted_records WHERE vault_id=$1")
            .bind(vault)
            .fetch_one(&server.pool)
            .await
            .unwrap();
    assert_eq!(retained, 0);

    // A fresh client imports the fenced snapshot.
    let fresh = open("fresh.db", Uuid::new_v4());
    let (_, start) = server.get(&server.owner_token, "/v1/snapshot").await;
    let generation: u64 = start["compaction_generation"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(generation, 1);
    let high_water: u64 = start["high_water_cursor"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let count: u64 = start["record_count"].as_str().unwrap().parse().unwrap();
    assert_eq!(count, 0);
    let mut progress = fresh
        .begin_snapshot_with_compaction(
            core::Cursor(high_water),
            count,
            core::SnapshotPurpose::Resync,
            Some(generation),
        )
        .unwrap();
    while progress.received_records < progress.expected_records {
        let (status, page) = server.get(&server.owner_token, &format!(
            "/v1/snapshot/records?high_water={high_water}&compaction_generation={generation}&after={}", progress.last_cursor.0)).await;
        assert_eq!(status, StatusCode::OK);
        let records = page["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| core::RawSnapshotRecord {
                cursor: core::Cursor(r["cursor"].as_str().unwrap().parse().unwrap()),
                envelope_json: serde_json::to_vec(&r["envelope"]).unwrap(),
            })
            .collect::<Vec<_>>();
        progress = fresh
            .append_snapshot_raw_page(progress.generation, &records)
            .unwrap();
    }
    fresh.finish_snapshot(progress.generation).unwrap();
    while fresh.apply_pending(100).unwrap().snapshot_remaining > 0 {}
    assert!(
        fresh
            .notification_snapshot()
            .unwrap()
            .notifications
            .is_empty()
    );
    // A stale pre-purge fence is refused rather than silently partial.
    let (status, restart) = server
        .get(
            &server.owner_token,
            &format!(
                "/v1/snapshot/records?high_water={high_water}&compaction_generation=0&after=0"
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(restart["reason"], "compaction_generation_changed");
    // A retry of a purged legacy post stays a duplicate.
    let (status, again) = server
        .post(
            &server.owner_token,
            "/v1/events",
            &serde_json::to_value(&legacy[0]).unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["duplicate"], true);
    server.shutdown().await;
}
