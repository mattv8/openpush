use std::time::Duration;

use peppy_server::{
    api::{TransportOptions, create_owner, router_with_options},
    config::Config,
    storage::{Storage, StorageError},
};
use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;
use uuid::Uuid;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TestServer {
    pool: PgPool,
    admin: PgPool,
    schema: String,
    base_url: String,
    task: JoinHandle<()>,
    owner_token: String,
    vault: Uuid,
    storage: Storage,
}

impl TestServer {
    async fn start() -> Self {
        let database_url = std::env::var("TEST_DATABASE_URL")
            .expect("TEST_DATABASE_URL is required; integration tests never silently skip");
        let config = Config::from_env().expect("complete S3 test configuration is required");
        let storage = Storage::new(
            config
                .s3
                .as_ref()
                .expect("S3 is required for attachment tests"),
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .unwrap();
        let schema = format!("peppy_attachment_test_{}", Uuid::new_v4().simple());
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

        let vault = Uuid::new_v4();
        let (profile, fingerprint) = profile(vault);
        let owner = create_owner(&pool, profile, vec![1, 2, 3], fingerprint, 1)
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_pool = pool.clone();
        let task = tokio::spawn(async move {
            // Fast storage maintenance so cleanup is observable within the test.
            let options = TransportOptions {
                storage_cleanup_interval: Duration::from_millis(200),
                ..TransportOptions::default()
            };
            axum::serve(listener, router_with_options(server_pool, options))
                .await
                .unwrap();
        });
        Self {
            pool,
            admin,
            schema,
            base_url: format!("http://{addr}"),
            task,
            owner_token: owner.device_token,
            vault,
            storage,
        }
    }

    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.bearer_auth(&self.owner_token)
    }

    async fn reserve(
        &self,
        bytes: &[u8],
        declared_bytes: Option<i64>,
        hash: Option<String>,
    ) -> Response {
        self.auth(Client::new().post(format!("{}/v1/attachments/reserve", self.base_url)))
            .json(&json!({
                "declared_ciphertext_bytes": declared_bytes.unwrap_or(bytes.len() as i64),
                "declared_ciphertext_sha256": hash.unwrap_or_else(|| hex::encode(Sha256::digest(bytes))),
            }))
            .send()
            .await
            .unwrap()
    }

    async fn reserve_id(&self, bytes: &[u8]) -> Uuid {
        let response = self.reserve(bytes, None, None).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            response.text().await.unwrap()
        );
        response.json::<Value>().await.unwrap()["attachment_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn reserve_client_id(&self, attachment_id: Uuid, bytes: &[u8]) -> Response {
        self.auth(Client::new().post(format!("{}/v1/attachments/reserve", self.base_url)))
            .json(&json!({
                "attachment_id": attachment_id,
                "declared_ciphertext_bytes": bytes.len(),
                "declared_ciphertext_sha256": hex::encode(Sha256::digest(bytes)),
            }))
            .send()
            .await
            .unwrap()
    }

    async fn upload(&self, attachment: Uuid, bytes: impl Into<reqwest::Body>) -> Response {
        self.auth(Client::new().put(format!(
            "{}/v1/attachments/{attachment}/upload",
            self.base_url
        )))
        .body(bytes)
        .send()
        .await
        .unwrap()
    }

    async fn finalize(&self, attachment: Uuid) -> Response {
        self.auth(Client::new().post(format!(
            "{}/v1/attachments/{attachment}/finalize",
            self.base_url
        )))
        .send()
        .await
        .unwrap()
    }

    async fn add_device(&self, role: &str) -> String {
        let id = Uuid::new_v4();
        let token = (0..3)
            .map(|_| Uuid::new_v4().simple().to_string())
            .collect::<String>();
        let fingerprint: String =
            sqlx::query_scalar("SELECT profile_fingerprint FROM vaults WHERE vault_id=$1")
                .bind(self.vault)
                .fetch_one(&self.pool)
                .await
                .unwrap();
        sqlx::query("INSERT INTO devices(vault_id,device_id,role,public_key,profile_fingerprint,key_epoch) VALUES($1,$2,$3,$4,$5,1)")
            .bind(self.vault)
            .bind(id)
            .bind(role)
            .bind(json!({"test_device": true}))
            .bind(fingerprint)
            .execute(&self.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO device_credentials(token_digest,vault_id,device_id) VALUES($1,$2,$3)",
        )
        .bind(Sha256::digest(token.as_bytes()).as_slice())
        .bind(self.vault)
        .bind(id)
        .execute(&self.pool)
        .await
        .unwrap();
        token
    }

    async fn object_key(&self, attachment: Uuid) -> String {
        sqlx::query_scalar(
            "SELECT object_key FROM upload_reservations WHERE vault_id=$1 AND attachment_id=$2",
        )
        .bind(self.vault)
        .bind(attachment)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn shutdown(self) {
        let rows = sqlx::query("SELECT object_key FROM upload_reservations")
            .fetch_all(&self.pool)
            .await
            .unwrap();
        for row in rows {
            let key: String = row.get("object_key");
            let _ = self.storage.delete(&key).await;
        }
        let rows = sqlx::query("SELECT object_key FROM public_attachment_copies")
            .fetch_all(&self.pool)
            .await
            .unwrap();
        for row in rows {
            let key: String = row.get("object_key");
            let _ = self.storage.delete(&key).await;
        }
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

fn profile(vault: Uuid) -> (Value, String) {
    let salt: Vec<u8> = (0..16).collect();
    let mut digest = Sha256::new();
    digest.update(b"peppy-key-profile-v1\0");
    digest.update(1_u16.to_be_bytes());
    digest.update(&salt);
    digest.update(vault.as_bytes());
    digest.update(1_u32.to_be_bytes());
    (
        json!({"crypto_suite":1,"salt":salt,"vault_id":vault,"key_epoch":1}),
        hex::encode(digest.finalize()),
    )
}

async fn foreign_owner(pool: &PgPool) -> String {
    let vault = Uuid::new_v4();
    let (profile, fingerprint) = profile(vault);
    create_owner(pool, profile, vec![4, 5, 6], fingerprint, 1)
        .await
        .unwrap()
        .device_token
}

#[tokio::test]
async fn ciphertext_roundtrip_streams_through_real_postgres_router_and_seaweed() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let ciphertext = (0..256 * 1024)
        .map(|offset| (offset % 251) as u8)
        .collect::<Vec<_>>();
    let attachment = server.reserve_id(&ciphertext).await;
    let key = server.object_key(attachment).await;
    assert!(key.starts_with(&format!("private/{}/", server.vault)));

    assert_eq!(
        server.upload(attachment, ciphertext.clone()).await.status(),
        StatusCode::NO_CONTENT
    );
    let finalized: Value = server.finalize(attachment).await.json().await.unwrap();
    assert_eq!(
        finalized,
        json!({"attachment_id":attachment,"duplicate":false})
    );
    let duplicate: Value = server.finalize(attachment).await.json().await.unwrap();
    assert_eq!(
        duplicate,
        json!({"attachment_id":attachment,"duplicate":true})
    );

    let download = server
        .auth(Client::new().get(format!("{}/v1/attachments/{attachment}", server.base_url)))
        .send()
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(
        download.headers()["content-type"],
        "application/octet-stream"
    );
    assert_eq!(download.headers()["content-disposition"], "attachment");
    assert_eq!(download.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        download.bytes().await.unwrap().as_ref(),
        ciphertext.as_slice()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn private_attachment_routes_require_auth_and_do_not_cross_vaults() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let ciphertext = b"private ciphertext".to_vec();
    let attachment = server.reserve_id(&ciphertext).await;
    let foreign_token = foreign_owner(&server.pool).await;
    let foreign_upload = Client::new()
        .put(format!(
            "{}/v1/attachments/{attachment}/upload",
            server.base_url
        ))
        .bearer_auth(&foreign_token)
        .body(ciphertext.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(foreign_upload.status(), StatusCode::NOT_FOUND);
    let foreign_finalize = Client::new()
        .post(format!(
            "{}/v1/attachments/{attachment}/finalize",
            server.base_url
        ))
        .bearer_auth(&foreign_token)
        .send()
        .await
        .unwrap();
    assert_eq!(foreign_finalize.status(), StatusCode::NOT_FOUND);

    assert_eq!(
        server.upload(attachment, ciphertext).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(server.finalize(attachment).await.status(), StatusCode::OK);

    let unauthenticated = Client::new()
        .get(format!("{}/v1/attachments/{attachment}", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let idor = Client::new()
        .get(format!("{}/v1/attachments/{attachment}", server.base_url))
        .bearer_auth(foreign_token)
        .send()
        .await
        .unwrap();
    assert_eq!(idor.status(), StatusCode::NOT_FOUND);

    let unauthenticated_reserve = Client::new()
        .post(format!("{}/v1/attachments/reserve", server.base_url))
        .json(&json!({"declared_ciphertext_bytes":1,"declared_ciphertext_sha256":hex::encode(Sha256::digest([1]))}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated_reserve.status(), StatusCode::UNAUTHORIZED);
    server.shutdown().await;
}

#[tokio::test]
async fn size_hash_and_missing_object_failures_are_typed() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;

    let too_large = server.reserve(&[1], Some(64 * 1024 * 1024 + 1), None).await;
    assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let expected = b"1234";
    let overrun = server.reserve_id(expected).await;
    let chunked_body = reqwest::Body::wrap_stream(futures_util::stream::iter([
        Ok::<_, std::io::Error>(b"12".to_vec()),
        Ok::<_, std::io::Error>(b"345".to_vec()),
    ]));
    let response = server.upload(overrun, chunked_body).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let wrong_hash = server.reserve_id(expected).await;
    let response = server.upload(wrong_hash, b"4321".to_vec()).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "ciphertext_mismatch"
    );

    let missing_before_finalize = server.reserve_id(expected).await;
    assert_eq!(
        server
            .upload(missing_before_finalize, expected.to_vec())
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let key = server.object_key(missing_before_finalize).await;
    server.storage.delete(&key).await.unwrap();
    let response = server.finalize(missing_before_finalize).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "attachment_unavailable"
    );

    let missing_after_finalize = server.reserve_id(expected).await;
    assert_eq!(
        server
            .upload(missing_after_finalize, expected.to_vec())
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server.finalize(missing_after_finalize).await.status(),
        StatusCode::OK
    );
    let key = server.object_key(missing_after_finalize).await;
    server.storage.delete(&key).await.unwrap();
    let response = server
        .auth(Client::new().get(format!(
            "{}/v1/attachments/{missing_after_finalize}",
            server.base_url
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "attachment_unavailable"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn client_attachment_ids_are_idempotent_and_public_copies_are_separate_and_revocable() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let ciphertext = b"encrypted original".to_vec();
    let attachment = Uuid::new_v4();
    assert_eq!(
        server
            .reserve_client_id(attachment, &ciphertext)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        server
            .reserve_client_id(attachment, &ciphertext)
            .await
            .status(),
        StatusCode::OK
    );
    let conflicting = server
        .auth(Client::new().post(format!("{}/v1/attachments/reserve", server.base_url)))
        .json(&json!({"attachment_id":attachment,"declared_ciphertext_bytes":1,"declared_ciphertext_sha256":hex::encode(Sha256::digest([1]))}))
        .send().await.unwrap();
    assert_eq!(conflicting.status(), StatusCode::CONFLICT);
    assert_eq!(
        server.upload(attachment, ciphertext).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(server.finalize(attachment).await.status(), StatusCode::OK);

    let plaintext = b"\x89PNG\r\n\x1a\nplaintext-image".to_vec();
    let created = server
        .auth(Client::new().post(format!(
            "{}/v1/attachments/{attachment}/public-copies",
            server.base_url
        )))
        .header("x-file-name", "../photo.png")
        .body(plaintext.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(
        created.status(),
        StatusCode::OK,
        "{}",
        created.text().await.unwrap()
    );
    let created: Value = created.json().await.unwrap();
    let share_id = created["share_id"].as_str().unwrap();
    let token = created["token"].as_str().unwrap();
    assert_eq!(token.len(), 43);
    let public_url = format!("{}/file/mms-usercontent/{token}/photo.png", server.base_url);
    let public = Client::new().get(&public_url).send().await.unwrap();
    assert_eq!(public.status(), StatusCode::OK);
    assert_eq!(public.headers()["content-type"], "image/png");
    assert_eq!(public.headers()["x-content-type-options"], "nosniff");
    assert_eq!(public.bytes().await.unwrap().as_ref(), plaintext.as_slice());
    let revoke = server
        .auth(Client::new().post(format!(
            "{}/v1/public-copies/{share_id}/revoke",
            server.base_url
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        Client::new().get(public_url).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    let vault: Value = server
        .auth(Client::new().get(format!("{}/v1/vault", server.base_url)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        vault["device_id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap(),
        sqlx::query_scalar::<_, Uuid>(
            "SELECT device_id FROM device_credentials WHERE token_digest=$1"
        )
        .bind(Sha256::digest(server.owner_token.as_bytes()).as_slice())
        .fetch_one(&server.pool)
        .await
        .unwrap()
    );
    assert_eq!(vault["role"], "owner");
    server.shutdown().await;
}

#[tokio::test]
async fn concurrent_reservations_cannot_overbook_the_vault_quota() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let max_attachment = 64 * 1024 * 1024_i64;
    for marker in 0_u8..7 {
        let response = server
            .reserve(
                &[marker],
                Some(max_attachment),
                Some(hex::encode(Sha256::digest([marker]))),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for marker in [7_u8, 8_u8] {
        let barrier = barrier.clone();
        let base_url = server.base_url.clone();
        let token = server.owner_token.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            Client::new()
                .post(format!("{base_url}/v1/attachments/reserve"))
                .bearer_auth(token)
                .json(&json!({
                    "declared_ciphertext_bytes": max_attachment,
                    "declared_ciphertext_sha256": hex::encode(Sha256::digest([marker])),
                }))
                .send()
                .await
                .unwrap()
        }));
    }
    barrier.wait().await;
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.unwrap().status());
    }
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::PAYLOAD_TOO_LARGE]);
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM upload_reservations WHERE vault_id=$1 AND finalized_at IS NULL",
    )
    .bind(server.vault)
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert_eq!(live, 8);
    server.shutdown().await;
}

#[tokio::test]
async fn expired_cleanup_fences_an_upload_already_streaming() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let ciphertext = b"cleanup-race-ciphertext".to_vec();
    let attachment = server.reserve_id(&ciphertext).await;
    let key = server.object_key(attachment).await;
    let (polled_tx, polled_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let body_bytes = ciphertext.clone();
    let body = reqwest::Body::wrap_stream(futures_util::stream::once(async move {
        let _ = polled_tx.send(());
        let _ = release_rx.await;
        Ok::<_, std::io::Error>(body_bytes)
    }));
    let base_url = server.base_url.clone();
    let token = server.owner_token.clone();
    let upload = tokio::spawn(async move {
        Client::new()
            .put(format!("{base_url}/v1/attachments/{attachment}/upload"))
            .bearer_auth(token)
            .body(body)
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), polled_rx)
        .await
        .expect("server never began consuming the upload")
        .unwrap();

    sqlx::query("UPDATE upload_reservations SET expires_at=now()-interval '1 second' WHERE vault_id=$1 AND attachment_id=$2")
        .bind(server.vault)
        .bind(attachment)
        .execute(&server.pool)
        .await
        .unwrap();
    assert_eq!(
        server
            .reserve_id(b"cleanup trigger")
            .await
            .to_string()
            .len(),
        36
    );
    let _ = release_tx.send(());
    let response = upload.await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "upload_reservation_not_found"
    );
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM upload_reservations WHERE vault_id=$1 AND attachment_id=$2",
    )
    .bind(server.vault)
    .bind(attachment)
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
    assert!(server.storage.head_bytes(&key).await.is_err());
    assert_eq!(
        server.finalize(attachment).await.status(),
        StatusCode::NOT_FOUND
    );

    let retained_bytes = b"finalized objects survive cleanup".to_vec();
    let retained = server.reserve_id(&retained_bytes).await;
    assert_eq!(
        server.upload(retained, retained_bytes).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(server.finalize(retained).await.status(), StatusCode::OK);
    let retained_key = server.object_key(retained).await;
    sqlx::query("UPDATE upload_reservations SET expires_at=now()-interval '1 second' WHERE vault_id=$1 AND attachment_id=$2")
        .bind(server.vault)
        .bind(retained)
        .execute(&server.pool)
        .await
        .unwrap();
    server.reserve_id(b"second cleanup trigger").await;
    assert!(server.storage.head_bytes(&retained_key).await.is_ok());
    let retained_download = server
        .auth(Client::new().get(format!("{}/v1/attachments/{retained}", server.base_url)))
        .send()
        .await
        .unwrap();
    assert_eq!(retained_download.status(), StatusCode::OK);
    server.shutdown().await;
}

#[tokio::test]
async fn public_copies_reject_unsafe_media_expiry_names_and_cross_vault_idor() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let ciphertext = b"private source".to_vec();
    let attachment = server.reserve_id(&ciphertext).await;
    assert_eq!(
        server.upload(attachment, ciphertext).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(server.finalize(attachment).await.status(), StatusCode::OK);
    let device_token = server.add_device("device").await;

    for unsafe_body in [
        b"<html>active</html>".as_slice(),
        b"<svg onload='alert(1)'/>".as_slice(),
    ] {
        let response = server
            .auth(Client::new().post(format!(
                "{}/v1/attachments/{attachment}/public-copies",
                server.base_url
            )))
            .header("x-file-name", "unsafe.svg")
            .body(unsafe_body.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            response.json::<Value>().await.unwrap()["code"],
            "unsupported_public_media"
        );
    }
    let invalid_name = server
        .auth(Client::new().post(format!(
            "{}/v1/attachments/{attachment}/public-copies",
            server.base_url
        )))
        .header("x-file-name", "../..")
        .body(b"\x89PNG\r\n\x1a\nimage".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_name.status(), StatusCode::BAD_REQUEST);

    // Same-vault devices may derive a SEPARATE public copy of a finalized attachment they can
    // already read (for example a received MMS image); the copy belongs to the requester.
    let device_copy = Client::new()
        .post(format!(
            "{}/v1/attachments/{attachment}/public-copies",
            server.base_url
        ))
        .bearer_auth(&device_token)
        .header("x-file-name", "received.png")
        .body(b"\x89PNG\r\n\x1a\nreceived".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(device_copy.status(), StatusCode::OK);
    let device_copy: Value = device_copy.json().await.unwrap();
    let device_share: Uuid = device_copy["share_id"].as_str().unwrap().parse().unwrap();
    let device_url = format!(
        "{}/file/mms-usercontent/{}/received.png",
        server.base_url,
        device_copy["token"].as_str().unwrap()
    );
    let served = Client::new().get(&device_url).send().await.unwrap();
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(
        served.bytes().await.unwrap().as_ref(),
        b"\x89PNG\r\n\x1a\nreceived",
        "the public object is the requester's re-encoded copy, never the private original"
    );
    let creator: Uuid = sqlx::query_scalar(
        "SELECT created_by_device_id FROM public_attachment_copies WHERE share_id=$1",
    )
    .bind(device_share)
    .fetch_one(&server.pool)
    .await
    .unwrap();
    let device_id: Uuid = sqlx::query_scalar(
        "SELECT d.device_id FROM devices d JOIN device_credentials c ON c.vault_id=d.vault_id AND c.device_id=d.device_id WHERE d.vault_id=$1 AND d.role='device'",
    )
    .bind(server.vault)
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert_eq!(
        creator, device_id,
        "the copy is owned by the requesting device"
    );

    // Another vault still cannot see or copy this attachment.
    let foreign = foreign_owner(&server.pool).await;
    let cross_vault = Client::new()
        .post(format!(
            "{}/v1/attachments/{attachment}/public-copies",
            server.base_url
        ))
        .bearer_auth(&foreign)
        .header("x-file-name", "photo.png")
        .body(b"\x89PNG\r\n\x1a\nimage".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(cross_vault.status(), StatusCode::NOT_FOUND);
    // An unfinalized reservation is not an existing attachment, even for the owner.
    let pending = server.reserve_id(b"never finalized").await;
    let unfinalized = server
        .auth(Client::new().post(format!(
            "{}/v1/attachments/{pending}/public-copies",
            server.base_url
        )))
        .header("x-file-name", "photo.png")
        .body(b"\x89PNG\r\n\x1a\nimage".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(unfinalized.status(), StatusCode::NOT_FOUND);
    // Without credentials nothing can be created.
    let anonymous = Client::new()
        .post(format!(
            "{}/v1/attachments/{attachment}/public-copies",
            server.base_url
        ))
        .header("x-file-name", "photo.png")
        .body(b"\x89PNG\r\n\x1a\nimage".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let created = server
        .auth(Client::new().post(format!(
            "{}/v1/attachments/{attachment}/public-copies",
            server.base_url
        )))
        .header("x-file-name", "photo.png")
        .body(b"\x89PNG\r\n\x1a\nimage".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let created: Value = created.json().await.unwrap();
    let share_id: Uuid = created["share_id"].as_str().unwrap().parse().unwrap();
    let token = created["token"].as_str().unwrap();

    let wrong_name = Client::new()
        .get(format!(
            "{}/file/mms-usercontent/{token}/other.png",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_name.status(), StatusCode::NOT_FOUND);
    let non_owner_revoke = Client::new()
        .post(format!(
            "{}/v1/public-copies/{share_id}/revoke",
            server.base_url
        ))
        .bearer_auth(&device_token)
        .send()
        .await
        .unwrap();
    assert_eq!(non_owner_revoke.status(), StatusCode::NOT_FOUND);
    // Revocation rules are unchanged: the creating device may revoke its own copy.
    let own_revoke = Client::new()
        .post(format!(
            "{}/v1/public-copies/{device_share}/revoke",
            server.base_url
        ))
        .bearer_auth(&device_token)
        .send()
        .await
        .unwrap();
    assert_eq!(own_revoke.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        Client::new()
            .get(&device_url)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    sqlx::query("UPDATE public_attachment_copies SET expires_at=now()-interval '1 second' WHERE share_id=$1")
        .bind(share_id)
        .execute(&server.pool)
        .await
        .unwrap();
    let expired = Client::new()
        .get(format!(
            "{}/file/mms-usercontent/{token}/photo.png",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(expired.status(), StatusCode::NOT_FOUND);
    server.shutdown().await;
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

/// A request body whose single chunk is released by the returned sender; the
/// receiver resolves once the server starts consuming the body.
fn held_body(
    bytes: Vec<u8>,
) -> (
    reqwest::Body,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (polled_tx, polled_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let body = reqwest::Body::wrap_stream(futures_util::stream::once(async move {
        let _ = polled_tx.send(());
        let _ = release_rx.await;
        Ok::<_, std::io::Error>(bytes)
    }));
    (body, polled_rx, release_tx)
}

fn spool_entries() -> usize {
    let directory = std::env::temp_dir().join(format!("peppy-spool-{}", std::process::id()));
    std::fs::read_dir(directory).map_or(0, |entries| entries.count())
}

fn png(bytes: usize) -> Vec<u8> {
    let mut body = b"\x89PNG\r\n\x1a\n".to_vec();
    body.resize(bytes, 0);
    body
}

impl TestServer {
    async fn public_copy(&self, attachment: Uuid, body: Vec<u8>) -> Response {
        self.auth(Client::new().post(format!(
            "{}/v1/attachments/{attachment}/public-copies",
            self.base_url
        )))
        .header("x-file-name", "photo.png")
        .body(body)
        .send()
        .await
        .unwrap()
    }

    async fn finalized_attachment(&self, bytes: &[u8]) -> Uuid {
        let attachment = self.reserve_id(bytes).await;
        assert_eq!(
            self.upload(attachment, bytes.to_vec()).await.status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(self.finalize(attachment).await.status(), StatusCode::OK);
        attachment
    }

    async fn queued(&self, reason: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM storage_deletions WHERE vault_id=$1 AND reason=$2")
            .bind(self.vault)
            .bind(reason)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn release_as(
        &self,
        token: &str,
        attachment: Uuid,
        generation: &str,
        cursor: &str,
    ) -> StatusCode {
        Client::new()
            .delete(format!("{}/v1/attachments/{attachment}", self.base_url))
            .bearer_auth(token)
            .json(&json!({
                "compaction_generation": generation,
                "release_before_cursor": cursor,
            }))
            .send()
            .await
            .unwrap()
            .status()
    }

    async fn register_as(&self, token: &str, attachment: Uuid, references: Value) -> StatusCode {
        Client::new()
            .post(format!(
                "{}/v1/attachments/{attachment}/references",
                self.base_url
            ))
            .bearer_auth(token)
            .json(&json!({ "references": references }))
            .send()
            .await
            .unwrap()
            .status()
    }

    async fn tracked_attachment(&self, bytes: &[u8]) -> Uuid {
        let response = self
            .auth(Client::new().post(format!("{}/v1/attachments/reserve", self.base_url)))
            .json(&json!({
                "declared_ciphertext_bytes": bytes.len(),
                "declared_ciphertext_sha256": hex::encode(Sha256::digest(bytes)),
                "reference_tracking": true,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let attachment: Uuid = response.json::<Value>().await.unwrap()["attachment_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            self.upload(attachment, bytes.to_vec()).await.status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(self.finalize(attachment).await.status(), StatusCode::OK);
        attachment
    }

    async fn device_with_role(&self, role: &str) -> Uuid {
        sqlx::query_scalar("SELECT device_id FROM devices WHERE vault_id=$1 AND role=$2")
            .bind(self.vault)
            .bind(role)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn compact(&self, device: Uuid, sequence: i64, original_cursor: i64) {
        sqlx::query("INSERT INTO compacted_records(vault_id,producer_device_id,producer_sequence,envelope_id,cipher_digest,original_cursor) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(self.vault)
            .bind(device)
            .bind(sequence)
            .bind(Uuid::new_v4())
            .bind(vec![0_u8; 32])
            .bind(original_cursor)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    async fn attachment_exists(&self, attachment: Uuid) -> bool {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM attachments WHERE vault_id=$1 AND attachment_id=$2)",
        )
        .bind(self.vault)
        .bind(attachment)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

fn reference(device: Uuid, sequence: &str) -> Value {
    json!([{ "producer_device_id": device.to_string(), "producer_sequence": sequence }])
}

#[tokio::test]
async fn gateway_releases_tracked_attachment_only_after_every_reference_is_compacted() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let owner = server.owner_token.clone();
    let owner_device = server.device_with_role("owner").await;
    let gateway = server.add_device("gateway").await;
    let gateway_device = server.device_with_role("gateway").await;
    let foreign = foreign_owner(&server.pool).await;
    let foreign_device: Uuid =
        sqlx::query_scalar("SELECT device_id FROM device_credentials WHERE token_digest=$1")
            .bind(Sha256::digest(foreign.as_bytes()).as_slice())
            .fetch_one(&server.pool)
            .await
            .unwrap();
    let attachment = server.tracked_attachment(b"contact photo").await;
    let private_key: String = sqlx::query_scalar(
        "SELECT object_key FROM attachments WHERE vault_id=$1 AND attachment_id=$2",
    )
    .bind(server.vault)
    .bind(attachment)
    .fetch_one(&server.pool)
    .await
    .unwrap();
    let copy: Value = server
        .public_copy(attachment, png(32))
        .await
        .json()
        .await
        .unwrap();
    let copy_key: String =
        sqlx::query_scalar("SELECT object_key FROM public_attachment_copies WHERE share_id=$1")
            .bind(copy["share_id"].as_str().unwrap().parse::<Uuid>().unwrap())
            .fetch_one(&server.pool)
            .await
            .unwrap();

    // Registration: own producer only, canonical form, same vault only.
    assert_eq!(
        server
            .register_as(&owner, attachment, reference(gateway_device, "3"))
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .register_as(&owner, attachment, reference(owner_device, "07"))
            .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server
            .register_as(&owner, attachment, reference(owner_device, "+7"))
            .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server
            .register_as(&owner, attachment, reference(owner_device, "0"))
            .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server.register_as(&owner, attachment, json!([{ "producer_device_id": owner_device.to_string().to_uppercase(), "producer_sequence": "7" }])
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server.register_as(&owner, attachment, json!([])).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server
            .register_as(&foreign, attachment, reference(foreign_device, "7"))
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        server
            .register_as(&owner, attachment, reference(owner_device, "7"))
            .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server
            .register_as(&owner, attachment, reference(owner_device, "7"))
            .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server
            .register_as(&gateway, attachment, reference(gateway_device, "3"))
            .await,
        StatusCode::NO_CONTENT
    );
    let registered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM attachment_record_references WHERE vault_id=$1 AND attachment_id=$2",
    )
    .bind(server.vault)
    .bind(attachment)
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert_eq!(registered, 2);

    // An unrelated retained immutable record at a low cursor must not block.
    sqlx::query("UPDATE vaults SET next_cursor=10,replay_floor_cursor=10,compaction_generation=1 WHERE vault_id=$1")
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO encrypted_records(vault_id,producer_device_id,envelope_id,cursor,producer_sequence,purpose,command_id,cipher_digest,envelope) VALUES($1,$2,$3,1,1,'event',NULL,$4,$5)")
        .bind(server.vault)
        .bind(owner_device)
        .bind(Uuid::new_v4())
        .bind(vec![1_u8; 32])
        .bind(json!({"unrelated": true}))
        .execute(&server.pool)
        .await
        .unwrap();
    server.compact(owner_device, 7, 5).await;

    // The gateway reference is still pending (never compacted): blocked.
    assert_eq!(
        server.release_as(&gateway, attachment, "1", "10").await,
        StatusCode::CONFLICT
    );
    server.compact(gateway_device, 3, 9).await;
    // Cutoff below a reference's original cursor, above the replay floor, or a
    // stale/zero generation are not proofs.
    assert_eq!(
        server.release_as(&gateway, attachment, "1", "8").await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        server.release_as(&gateway, attachment, "1", "11").await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        server.release_as(&gateway, attachment, "2", "9").await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        server.release_as(&gateway, attachment, "0", "9").await,
        StatusCode::CONFLICT
    );
    assert!(server.attachment_exists(attachment).await);
    // Another vault cannot see or release it.
    assert_eq!(
        server.release_as(&foreign, attachment, "1", "9").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(server.queued("finalized_attachment").await, 0);

    // The gateway (not the owner) completes the proven release; repeats are no-ops.
    assert_eq!(
        server.release_as(&gateway, attachment, "1", "9").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server.release_as(&gateway, attachment, "1", "9").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server.release_as(&owner, attachment, "1", "9").await,
        StatusCode::NO_CONTENT
    );
    assert!(!server.attachment_exists(attachment).await);
    assert_eq!(server.queued("finalized_attachment").await, 1);
    assert_eq!(
        server.release_as(&foreign, attachment, "1", "9").await,
        StatusCode::NOT_FOUND
    );

    // A late registration cannot resurrect the released attachment.
    assert_eq!(
        server
            .register_as(&owner, attachment, reference(owner_device, "8"))
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        server
            .auth(Client::new().get(format!("{}/v1/attachments/{attachment}", server.base_url)))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    wait_until(10, "the released private object to be deleted", || async {
        matches!(
            server.storage.head_bytes(&private_key).await,
            Err(StorageError::NotFound)
        )
    })
    .await;
    assert!(
        server.storage.head_bytes(&copy_key).await.is_ok(),
        "public copy is independent"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn untracked_or_unreferenced_attachments_are_never_released() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let owner = server.owner_token.clone();
    let owner_device = server.device_with_role("owner").await;
    let legacy = server.finalized_attachment(b"legacy photo").await;
    let unreferenced = server.tracked_attachment(b"unreferenced photo").await;
    sqlx::query("UPDATE vaults SET next_cursor=10,replay_floor_cursor=10,compaction_generation=1 WHERE vault_id=$1")
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    server.compact(owner_device, 1, 1).await;

    // Legacy untracked attachments cannot gain references or be auto-released.
    assert_eq!(
        server
            .register_as(&owner, legacy, reference(owner_device, "1"))
            .await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        server.release_as(&owner, legacy, "1", "10").await,
        StatusCode::CONFLICT
    );
    assert!(server.attachment_exists(legacy).await);

    // A tracked attachment with no registered reference has no proof.
    assert_eq!(
        server.release_as(&owner, unreferenced, "1", "10").await,
        StatusCode::CONFLICT
    );
    assert!(server.attachment_exists(unreferenced).await);

    // Unknown attachments are not found, for release and registration alike.
    let unknown = Uuid::new_v4();
    assert_eq!(
        server.release_as(&owner, unknown, "1", "10").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        server
            .register_as(&owner, unknown, reference(owner_device, "1"))
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(server.queued("finalized_attachment").await, 0);
    server.shutdown().await;
}

#[tokio::test]
async fn late_duplicate_upload_cannot_delete_the_finalized_object() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let ciphertext = b"duplicate-put-ciphertext".to_vec();
    let attachment = server.reserve_id(&ciphertext).await;

    // PUT-A starts and is held mid-body.
    let (body, polled, release) = held_body(ciphertext.clone());
    let base_url = server.base_url.clone();
    let token = server.owner_token.clone();
    let late = tokio::spawn(async move {
        Client::new()
            .put(format!("{base_url}/v1/attachments/{attachment}/upload"))
            .bearer_auth(token)
            .body(body)
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), polled)
        .await
        .expect("client never began sending PUT-A")
        .unwrap();
    // PUT-A has passed its reservation lookup once its spool exists.
    wait_until(5, "PUT-A to start spooling", || async {
        spool_entries() == 1
    })
    .await;

    // PUT-B completes and is finalized while A is still in flight.
    assert_eq!(
        server.upload(attachment, ciphertext.clone()).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(server.finalize(attachment).await.status(), StatusCode::OK);
    let finalized_key: String = sqlx::query_scalar(
        "SELECT object_key FROM attachments WHERE vault_id=$1 AND attachment_id=$2",
    )
    .bind(server.vault)
    .bind(attachment)
    .fetch_one(&server.pool)
    .await
    .unwrap();

    // Releasing A: it may only fail and clean its own attempt key.
    let _ = release.send(());
    let response = late.await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    wait_until(10, "the late attempt object to be deleted", || async {
        server.queued("upload_attempt").await == 0
    })
    .await;
    assert_eq!(
        server.storage.head_bytes(&finalized_key).await,
        Ok(ciphertext.len() as i64)
    );
    let download = server
        .auth(Client::new().get(format!("{}/v1/attachments/{attachment}", server.base_url)))
        .send()
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(
        download.bytes().await.unwrap().as_ref(),
        ciphertext.as_slice()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn reupload_before_finalize_supersedes_and_deletes_only_the_old_attempt() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let ciphertext = b"reupload-ciphertext".to_vec();
    let attachment = server.reserve_id(&ciphertext).await;
    assert_eq!(
        server.upload(attachment, ciphertext.clone()).await.status(),
        StatusCode::NO_CONTENT
    );
    let first_key = server.object_key(attachment).await;
    assert_eq!(
        server.upload(attachment, ciphertext.clone()).await.status(),
        StatusCode::NO_CONTENT
    );
    let second_key = server.object_key(attachment).await;
    assert_ne!(first_key, second_key);
    wait_until(10, "the superseded attempt to be deleted", || async {
        matches!(
            server.storage.head_bytes(&first_key).await,
            Err(StorageError::NotFound)
        )
    })
    .await;
    assert_eq!(server.finalize(attachment).await.status(), StatusCode::OK);
    assert!(server.storage.head_bytes(&second_key).await.is_ok());
    server.shutdown().await;
}

#[tokio::test]
async fn deletion_queue_never_deletes_referenced_objects_and_retries_failures() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let attachment = server.finalized_attachment(b"referenced object").await;
    let key = server.object_key(attachment).await;
    // A stale queue entry for a finalized object must be dropped, not executed.
    sqlx::query("INSERT INTO storage_deletions(object_key,vault_id,reason) VALUES($1,$2,'superseded_upload')")
        .bind(&key)
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    wait_until(10, "the stale entry to be dropped", || async {
        server.queued("superseded_upload").await == 0
    })
    .await;
    assert!(server.storage.head_bytes(&key).await.is_ok());

    // A failing delete is retried with backoff instead of being forgotten.
    let invalid = format!("private/{}/../escape", server.vault);
    sqlx::query("INSERT INTO storage_deletions(object_key,vault_id,reason) VALUES($1,$2,'expired_reservation')")
        .bind(&invalid)
        .bind(server.vault)
        .execute(&server.pool)
        .await
        .unwrap();
    wait_until(10, "a failed delete to be rescheduled", || async {
        sqlx::query_scalar::<_, bool>("SELECT attempts >= 1 AND not_before > now() FROM storage_deletions WHERE object_key=$1")
            .bind(&invalid)
            .fetch_optional(&server.pool)
            .await
            .unwrap()
            .unwrap_or(false)
    })
    .await;

    // Expired uploaded reservations are cleaned by the background worker even
    // when no new reservation is made.
    let bytes = b"expired background cleanup".to_vec();
    let expired = server.reserve_id(&bytes).await;
    assert_eq!(
        server.upload(expired, bytes).await.status(),
        StatusCode::NO_CONTENT
    );
    let expired_key = server.object_key(expired).await;
    sqlx::query("UPDATE upload_reservations SET expires_at=now()-interval '1 second' WHERE vault_id=$1 AND attachment_id=$2")
        .bind(server.vault)
        .bind(expired)
        .execute(&server.pool)
        .await
        .unwrap();
    wait_until(10, "the expired object to be deleted", || async {
        matches!(
            server.storage.head_bytes(&expired_key).await,
            Err(StorageError::NotFound)
        )
    })
    .await;
    sqlx::query("DELETE FROM storage_deletions WHERE object_key=$1")
        .bind(&invalid)
        .execute(&server.pool)
        .await
        .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn public_copies_count_against_quota_and_cleanup_removes_their_objects() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let source = b"public source".to_vec();
    let attachment = server.finalized_attachment(&source).await;
    let mib = 1024 * 1024_i64;
    for marker in 0_u8..7 {
        let response = server
            .reserve(
                &[marker],
                Some(64 * mib),
                Some(hex::encode(Sha256::digest([marker]))),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    // Leaves 15 MiB minus the source: room for exactly one 8 MiB copy.
    let response = server
        .reserve(&[9], Some(49 * mib), Some(hex::encode(Sha256::digest([9]))))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let barrier = barrier.clone();
        let url = format!(
            "{}/v1/attachments/{attachment}/public-copies",
            server.base_url
        );
        let token = server.owner_token.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let response = Client::new()
                .post(url)
                .bearer_auth(token)
                .header("x-file-name", "photo.png")
                .body(png(8 * 1024 * 1024))
                .send()
                .await
                .unwrap();
            let status = response.status();
            (status, response.json::<Value>().await.unwrap())
        }));
    }
    barrier.wait().await;
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.unwrap());
    }
    results.sort_by_key(|(status, _)| *status);
    assert_eq!(results[0].0, StatusCode::OK, "{}", results[0].1);
    assert_eq!(results[1].0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(results[1].1["code"], "vault_quota_exceeded");
    let winner = &results[0].1;
    let share_id: Uuid = winner["share_id"].as_str().unwrap().parse().unwrap();
    let copies: i64 =
        sqlx::query_scalar("SELECT count(*) FROM public_attachment_copies WHERE vault_id=$1")
            .bind(server.vault)
            .fetch_one(&server.pool)
            .await
            .unwrap();
    assert_eq!(copies, 1, "the rejected copy must leave no row");
    let copy_key = |share: Uuid| {
        let pool = server.pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT object_key FROM public_attachment_copies WHERE share_id=$1",
            )
            .bind(share)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let key = copy_key(share_id).await;
    assert!(server.storage.head_bytes(&key).await.is_ok());

    let revoke = server
        .auth(Client::new().post(format!(
            "{}/v1/public-copies/{share_id}/revoke",
            server.base_url
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::NO_CONTENT);
    wait_until(10, "the revoked copy object to be deleted", || async {
        matches!(
            server.storage.head_bytes(&key).await,
            Err(StorageError::NotFound)
        )
    })
    .await;
    wait_until(5, "the revoked copy to be purged", || async {
        sqlx::query_scalar::<_, bool>(
            "SELECT purged_at IS NOT NULL FROM public_attachment_copies WHERE share_id=$1",
        )
        .bind(share_id)
        .fetch_one(&server.pool)
        .await
        .unwrap()
    })
    .await;

    // Purged bytes are released, so a new copy fits; expiry then cleans it too.
    let replacement = server.public_copy(attachment, png(8 * 1024 * 1024)).await;
    assert_eq!(replacement.status(), StatusCode::OK);
    let replacement: Value = replacement.json().await.unwrap();
    let replacement_id: Uuid = replacement["share_id"].as_str().unwrap().parse().unwrap();
    let replacement_key = copy_key(replacement_id).await;
    sqlx::query("UPDATE public_attachment_copies SET expires_at=now()-interval '1 second' WHERE share_id=$1")
        .bind(replacement_id)
        .execute(&server.pool)
        .await
        .unwrap();
    wait_until(10, "the expired copy object to be deleted", || async {
        matches!(
            server.storage.head_bytes(&replacement_key).await,
            Err(StorageError::NotFound)
        )
    })
    .await;
    server.shutdown().await;
}

#[tokio::test]
async fn spool_files_are_removed_on_every_failure_and_cancellation_path() {
    let _guard = TEST_LOCK.lock().await;
    let server = TestServer::start().await;
    let expected = b"1234";
    let overrun = server.reserve_id(expected).await;
    assert_eq!(
        server.upload(overrun, b"12345".to_vec()).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(spool_entries(), 0, "overrun");
    let mismatch = server.reserve_id(expected).await;
    assert_eq!(
        server.upload(mismatch, b"4321".to_vec()).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(spool_entries(), 0, "hash mismatch");

    let attachment = server.finalized_attachment(b"spool source").await;
    assert_eq!(
        server
            .public_copy(attachment, b"<svg/>".to_vec())
            .await
            .status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(spool_entries(), 0, "unsupported media");
    assert_eq!(
        server
            .public_copy(attachment, png(10 * 1024 * 1024 + 1))
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(spool_entries(), 0, "oversized public copy");

    // A client that disappears mid-body cancels the handler; the spool goes too.
    let aborted = server.reserve_id(b"abandoned upload").await;
    let (body, polled, _release) = held_body(b"abandoned upload".to_vec());
    let base_url = server.base_url.clone();
    let token = server.owner_token.clone();
    let request = tokio::spawn(async move {
        let _ = Client::new()
            .put(format!("{base_url}/v1/attachments/{aborted}/upload"))
            .bearer_auth(token)
            .body(body)
            .send()
            .await;
    });
    tokio::time::timeout(Duration::from_secs(5), polled)
        .await
        .expect("server never began consuming the upload")
        .unwrap();
    wait_until(5, "the in-flight upload to own one spool file", || async {
        spool_entries() == 1
    })
    .await;
    request.abort();
    let _ = request.await;
    wait_until(10, "the cancelled upload spool to be removed", || async {
        spool_entries() == 0
    })
    .await;
    server.shutdown().await;
}
