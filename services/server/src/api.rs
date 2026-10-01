mod attachments;
mod key_profiles;
mod sync;

pub use sync::{DrainStats, TransportOptions, drain_outbox, prune_replay_log};

use axum::{
    Json, Router,
    extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use openpush_domain::{DeviceId, VaultId};
use openpush_protocol::{Envelope, EnvelopePurpose, pairing_proof_message};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::{config::Config, storage::Storage};

const TOKEN_BYTES_HEX: usize = 96;

#[derive(Clone)]
struct ApiState {
    db: PgPool,
    committed: tokio::sync::broadcast::Sender<(Uuid, i64)>,
    storage: Option<Storage>,
    vault_attachment_quota_bytes: i64,
    upload_slots: Arc<tokio::sync::Semaphore>,
    options: Arc<TransportOptions>,
}

/// Capacity of the in-process commit-hint channel shared by all sockets.
const HINT_CAPACITY: usize = 1024;

/// Builds the API with transport policy from the environment
/// (`OPENPUSH_REPLAY_RETENTION_DAYS`, default 30) and starts the bounded
/// in-process outbox/retention maintenance task.
pub fn router(db: PgPool) -> Router {
    let config = Config::from_env().ok();
    let mut options = TransportOptions::default();
    if let Some(config) = &config {
        options.replay_retention = config.replay_retention;
    }
    build_router(db, config, options)
}

/// Same as [`router`] with explicit transport timing/retention policy.
pub fn router_with_options(db: PgPool, options: TransportOptions) -> Router {
    build_router(db, Config::from_env().ok(), options)
}

fn build_router(db: PgPool, config: Option<Config>, options: TransportOptions) -> Router {
    let (committed, _) = tokio::sync::broadcast::channel(HINT_CAPACITY);
    let storage = config
        .as_ref()
        .and_then(|config| config.s3.as_ref().map(Storage::new));
    sync::spawn_maintenance(db.clone(), &committed, storage.clone(), options.clone());
    let vault_attachment_quota_bytes = config
        .as_ref()
        .map(|config| config.vault_attachment_quota_bytes)
        .unwrap_or(512 * 1024 * 1024);
    Router::new()
        .route("/v1/vault", get(vault_header))
        .route("/v1/pairing", post(create_pairing))
        .route("/v1/pairing/consume", post(consume_pairing))
        .route("/v1/devices/{device_id}/revoke", post(revoke_device))
        .route("/v1/devices", get(devices))
        .route(
            "/v1/capabilities",
            get(capabilities).post(update_capabilities),
        )
        .route("/v1/events", post(ingest))
        .route("/v1/commands", post(ingest))
        .route("/v1/events", get(sync::events))
        .route("/v1/snapshot", get(sync::snapshot_start))
        .route("/v1/snapshot/records", get(sync::snapshot_records))
        .route("/v1/commands/pending", get(sync::pending_commands))
        .route(
            "/v1/vault/key-profiles",
            get(key_profiles::list).post(key_profiles::register),
        )
        .route(
            "/v1/vault/key-profiles/{key_epoch}/activate",
            post(key_profiles::activate),
        )
        .route("/v1/commands/{command_id}/receipts", post(receipt))
        .route(
            "/v1/attachments/reserve",
            post(attachments::reserve_attachment),
        )
        .route(
            "/v1/attachments/{attachment_id}/upload",
            put(attachments::upload_attachment),
        )
        .route(
            "/v1/attachments/{attachment_id}/finalize",
            post(attachments::finalize_attachment),
        )
        .route(
            "/v1/attachments/{attachment_id}",
            get(attachments::download_attachment),
        )
        .route(
            "/v1/attachments/{attachment_id}/public-copies",
            post(attachments::create_public_copy),
        )
        .route(
            "/v1/public-copies/{share_id}/revoke",
            post(attachments::revoke_public_copy),
        )
        .route(
            "/file/mms-usercontent/{token}/{safe_name}",
            get(attachments::download_public_copy),
        )
        .route("/v1/ws", get(websocket))
        .with_state(ApiState {
            db,
            committed,
            storage,
            vault_attachment_quota_bytes,
            upload_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            options: Arc::new(options),
        })
}

#[derive(Debug)]
struct ApiError(StatusCode, &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({"code":self.1}))).into_response()
    }
}
type ApiResult<T> = Result<T, ApiError>;

fn database_unavailable(error: &sqlx::Error, operation: &'static str) -> ApiError {
    tracing::warn!(operation, error_kind = %error, "database operation failed");
    ApiError(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable")
}

fn insert_error(error: sqlx::Error, operation: &'static str) -> ApiError {
    unique_conflict(error, "idempotency_conflict", operation)
}

/// A unique violation is a client conflict with `code`; anything else is a
/// logged transient database failure.
fn unique_conflict(error: sqlx::Error, code: &'static str, operation: &'static str) -> ApiError {
    if error
        .as_database_error()
        .and_then(|database| database.code())
        .is_some_and(|code| code == "23505")
    {
        ApiError(StatusCode::CONFLICT, code)
    } else {
        database_unavailable(&error, operation)
    }
}

#[derive(Clone)]
struct Principal {
    vault: Uuid,
    device: Uuid,
    role: String,
}
async fn auth(db: &PgPool, headers: &HeaderMap) -> ApiResult<Principal> {
    let bearer = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "missing_bearer"))?;
    if bearer.len() != TOKEN_BYTES_HEX || !bearer.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "invalid_bearer"));
    }
    let digest = Sha256::digest(bearer.as_bytes());
    let row = sqlx::query("SELECT c.vault_id,c.device_id,d.role FROM device_credentials c JOIN devices d ON d.vault_id=c.vault_id AND d.device_id=c.device_id WHERE c.token_digest=$1 AND c.revoked_at IS NULL AND d.revoked_at IS NULL")
        .bind(digest.as_slice()).fetch_optional(db).await.map_err(|error| database_unavailable(&error, "api_query"))?.ok_or(ApiError(StatusCode::UNAUTHORIZED,"invalid_bearer"))?;
    Ok(Principal {
        vault: row.get("vault_id"),
        device: row.get("device_id"),
        role: row.get("role"),
    })
}
fn owner(p: &Principal) -> ApiResult<()> {
    if p.role == "owner" {
        Ok(())
    } else {
        Err(ApiError(StatusCode::FORBIDDEN, "owner_required"))
    }
}
fn credential_token() -> String {
    (0..3)
        .map(|_| Uuid::new_v4().simple().to_string())
        .collect()
}
fn challenge_token() -> String {
    let mut hasher = Sha256::new();
    for _ in 0..3 {
        hasher.update(Uuid::new_v4().as_bytes());
    }
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}
fn decode_challenge(value: &str) -> Option<[u8; 32]> {
    if value.len() != 43 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    bytes.try_into().ok()
}
fn profile_ok(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate_profile(profile: &Value, fingerprint: &str, epoch: u32) -> Result<Uuid, &'static str> {
    if !profile_ok(fingerprint) || epoch == 0 {
        return Err("invalid profile fingerprint or epoch");
    }
    let object = profile
        .as_object()
        .ok_or("public key profile must be an object")?;
    if object.get("crypto_suite").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported crypto suite");
    }
    if object.get("key_epoch").and_then(Value::as_u64) != Some(epoch.into()) {
        return Err("profile epoch mismatch");
    }
    let vault: Uuid = object
        .get("vault_id")
        .and_then(Value::as_str)
        .ok_or("profile vault id missing")?
        .parse()
        .map_err(|_| "invalid profile vault id")?;
    let salt = object
        .get("salt")
        .and_then(Value::as_array)
        .ok_or("profile salt missing")?;
    if salt.len() != 16 || salt.iter().any(|v| v.as_u64().is_none_or(|b| b > 255)) {
        return Err("invalid profile salt");
    }
    let mut digest = Sha256::new();
    digest.update(b"openpush-key-profile-v1\0");
    digest.update(1u16.to_be_bytes());
    for byte in salt {
        digest.update([byte.as_u64().expect("validated") as u8]);
    }
    digest.update(vault.as_bytes());
    digest.update(epoch.to_be_bytes());
    if hex::encode(digest.finalize()) != fingerprint {
        return Err("profile fingerprint mismatch");
    }
    Ok(vault)
}
fn verifying_key(value: &Value) -> Option<VerifyingKey> {
    let key = value.get("ed25519_public_key")?.as_str()?;
    let bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(key).ok()?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Local-admin bootstrap used only by the CLI. The caller supplies nonsecret crypto metadata;
/// the server never receives or derives a passphrase.
pub async fn create_owner(
    db: &PgPool,
    public_key_profile: Value,
    check_header: Vec<u8>,
    profile_fingerprint: String,
    key_epoch: u32,
) -> Result<Credential, String> {
    let vault = validate_profile(&public_key_profile, &profile_fingerprint, key_epoch)
        .map_err(str::to_owned)?;
    if check_header.is_empty() || check_header.len() > 1_048_576 {
        return Err("invalid encrypted vault check header".into());
    }
    let device = Uuid::new_v4();
    let t = credential_token();
    let digest = Sha256::digest(t.as_bytes());
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("INSERT INTO vaults(vault_id,public_key_profile,encrypted_vault_check_header,key_epoch,profile_fingerprint) VALUES($1,$2,$3,$4,$5)").bind(vault).bind(&public_key_profile).bind(check_header).bind(key_epoch as i32).bind(&profile_fingerprint).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    sqlx::query("INSERT INTO vault_key_profiles(vault_id,key_epoch,public_key_profile,encrypted_vault_check_header,profile_fingerprint,created_by_device_id,activated_at) SELECT vault_id,key_epoch,public_key_profile,encrypted_vault_check_header,profile_fingerprint,$2,now() FROM vaults WHERE vault_id=$1").bind(vault).bind(device).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    sqlx::query("INSERT INTO devices(vault_id,device_id,role,public_key,profile_fingerprint,key_epoch) VALUES($1,$2,'owner',$3,$4,$5)").bind(vault).bind(device).bind(public_key_profile).bind(&profile_fingerprint).bind(key_epoch as i32).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    sqlx::query("INSERT INTO device_credentials(token_digest,vault_id,device_id) VALUES($1,$2,$3)")
        .bind(digest.as_slice())
        .bind(vault)
        .bind(device)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(Credential {
        device_token: t,
        vault_id: vault,
        device_id: device,
        role: "owner".into(),
    })
}

#[derive(Serialize)]
struct VaultResponse {
    vault_id: Uuid,
    public_key_profile: Value,
    encrypted_vault_check_header: String,
    key_epoch: u32,
    profile_fingerprint: String,
    device_id: Uuid,
    role: String,
}
async fn vault_header(State(s): State<ApiState>, h: HeaderMap) -> ApiResult<Json<VaultResponse>> {
    let p = auth(&s.db, &h).await?;
    // Encoded here: PostgreSQL's encode(...,'base64') inserts a newline every 76 characters.
    let r=sqlx::query("SELECT vault_id,public_key_profile,encrypted_vault_check_header,key_epoch,profile_fingerprint FROM vaults WHERE vault_id=$1").bind(p.vault).fetch_one(&s.db).await.map_err(|error| database_unavailable(&error, "vault_header"))?;
    Ok(Json(VaultResponse {
        vault_id: r.get("vault_id"),
        public_key_profile: r.get("public_key_profile"),
        encrypted_vault_check_header: STANDARD
            .encode(r.get::<Vec<u8>, _>("encrypted_vault_check_header")),
        key_epoch: r.get::<i32, _>("key_epoch") as u32,
        profile_fingerprint: r.get("profile_fingerprint"),
        device_id: p.device,
        role: p.role,
    }))
}

#[derive(Deserialize)]
struct PairRequest {
    device_id: Uuid,
    public_key: Value,
    profile_fingerprint: String,
    key_epoch: u32,
    requested_role: String,
}
#[derive(Serialize)]
struct PairResponse {
    challenge_token: String,
    expires_in_seconds: u16,
    vault_id: Uuid,
    key_epoch: u32,
    profile_fingerprint: String,
    requested_role: String,
}
async fn create_pairing(
    State(s): State<ApiState>,
    h: HeaderMap,
    Json(x): Json<PairRequest>,
) -> ApiResult<Json<PairResponse>> {
    let p = auth(&s.db, &h).await?;
    owner(&p)?;
    if !matches!(x.requested_role.as_str(), "device" | "gateway")
        || verifying_key(&x.public_key).is_none()
    {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_pairing_request",
        ));
    }
    let v = sqlx::query("SELECT key_epoch,profile_fingerprint FROM vaults WHERE vault_id=$1")
        .bind(p.vault)
        .fetch_one(&s.db)
        .await
        .map_err(|error| database_unavailable(&error, "pairing_vault_lookup"))?;
    if v.get::<i32, _>("key_epoch") as u32 != x.key_epoch
        || v.get::<String, _>("profile_fingerprint") != x.profile_fingerprint
    {
        return Err(ApiError(StatusCode::CONFLICT, "profile_or_epoch_mismatch"));
    }
    let t = challenge_token();
    let d = Sha256::digest(t.as_bytes());
    sqlx::query("INSERT INTO pairing_challenges(challenge_digest,vault_id,requested_device_id,requested_public_key,approved_by_device_id,profile_fingerprint,key_epoch,requested_role,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,now()+interval '120 seconds')").bind(d.as_slice()).bind(p.vault).bind(x.device_id).bind(x.public_key).bind(p.device).bind(&x.profile_fingerprint).bind(x.key_epoch as i32).bind(&x.requested_role).execute(&s.db).await.map_err(|error| unique_conflict(error, "pairing_exists", "pairing_insert"))?;
    Ok(Json(PairResponse {
        challenge_token: t,
        expires_in_seconds: 120,
        vault_id: p.vault,
        key_epoch: x.key_epoch,
        profile_fingerprint: x.profile_fingerprint,
        requested_role: x.requested_role,
    }))
}
#[derive(Deserialize)]
struct ConsumePair {
    challenge_token: String,
    device_id: Uuid,
    public_key: Value,
    profile_fingerprint: String,
    key_epoch: u32,
    signature: String,
}
#[derive(Serialize)]
pub struct Credential {
    pub device_token: String,
    pub vault_id: Uuid,
    pub device_id: Uuid,
    pub role: String,
}
async fn consume_pairing(
    State(s): State<ApiState>,
    Json(x): Json<ConsumePair>,
) -> ApiResult<Json<Credential>> {
    let challenge = decode_challenge(&x.challenge_token)
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "invalid_challenge"))?;
    let d = Sha256::digest(x.challenge_token.as_bytes());
    let mut tx =
        s.db.begin()
            .await
            .map_err(|error| database_unavailable(&error, "pairing_consume_begin"))?;
    let r=sqlx::query("SELECT vault_id,requested_role FROM pairing_challenges WHERE challenge_digest=$1 AND consumed_at IS NULL AND expires_at>now() AND requested_device_id=$2 AND requested_public_key=$3 AND profile_fingerprint=$4 AND key_epoch=$5 FOR UPDATE").bind(d.as_slice()).bind(x.device_id).bind(&x.public_key).bind(&x.profile_fingerprint).bind(x.key_epoch as i32).fetch_optional(&mut *tx).await.map_err(|error| database_unavailable(&error, "pairing_consume_lookup"))?.ok_or(ApiError(StatusCode::UNAUTHORIZED,"challenge_invalid_or_consumed"))?;
    let vault: Uuid = r.get("vault_id");
    let role: String = r.get("requested_role");
    let key = verifying_key(&x.public_key).ok_or(ApiError(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_public_key",
    ))?;
    let signature: [u8; 64] = URL_SAFE_NO_PAD
        .decode(&x.signature)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "invalid_pairing_proof"))?;
    key.verify(
        &pairing_proof_message(
            &challenge,
            VaultId(vault),
            DeviceId(x.device_id),
            &x.profile_fingerprint,
            x.key_epoch,
            &role,
        ),
        &Signature::from_bytes(&signature),
    )
    .map_err(|_| ApiError(StatusCode::UNAUTHORIZED, "invalid_pairing_proof"))?;
    sqlx::query("UPDATE pairing_challenges SET consumed_at=now() WHERE challenge_digest=$1 AND consumed_at IS NULL").bind(d.as_slice()).execute(&mut *tx).await.map_err(|error| database_unavailable(&error, "pairing_consume_mark"))?;
    let t = credential_token();
    let td = Sha256::digest(t.as_bytes());
    sqlx::query("INSERT INTO devices(vault_id,device_id,role,public_key,profile_fingerprint,key_epoch) VALUES($1,$2,$3,$4,$5,$6)").bind(vault).bind(x.device_id).bind(&role).bind(x.public_key).bind(&x.profile_fingerprint).bind(x.key_epoch as i32).execute(&mut *tx).await.map_err(|error| unique_conflict(error, "device_exists", "pairing_device_insert"))?;
    sqlx::query("INSERT INTO device_credentials(token_digest,vault_id,device_id) VALUES($1,$2,$3)")
        .bind(td.as_slice())
        .bind(vault)
        .bind(x.device_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "pairing_credential_insert"))?;
    tx.commit()
        .await
        .map_err(|error| database_unavailable(&error, "pairing_consume_commit"))?;
    Ok(Json(Credential {
        device_token: t,
        vault_id: vault,
        device_id: x.device_id,
        role,
    }))
}

async fn revoke_device(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(device_id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let p = auth(&s.db, &h).await?;
    owner(&p)?;
    sqlx::query("UPDATE devices SET revoked_at=now() WHERE vault_id=$1 AND device_id=$2")
        .bind(p.vault)
        .bind(device_id)
        .execute(&s.db)
        .await
        .map_err(|error| database_unavailable(&error, "api_query"))?;
    sqlx::query(
        "UPDATE device_credentials SET revoked_at=now() WHERE vault_id=$1 AND device_id=$2",
    )
    .bind(p.vault)
    .bind(device_id)
    .execute(&s.db)
    .await
    .map_err(|error| database_unavailable(&error, "api_query"))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct DeviceView {
    device_id: Uuid,
    role: String,
    revoked: bool,
    profile_fingerprint: String,
    key_epoch: u32,
}
async fn devices(State(s): State<ApiState>, h: HeaderMap) -> ApiResult<Json<Value>> {
    let p = auth(&s.db, &h).await?;
    let rows = sqlx::query("SELECT device_id,role,revoked_at IS NOT NULL revoked,profile_fingerprint,key_epoch FROM devices WHERE vault_id=$1 ORDER BY created_at")
        .bind(p.vault).fetch_all(&s.db).await.map_err(|error| database_unavailable(&error, "api_query"))?;
    let devices = rows
        .into_iter()
        .map(|row| DeviceView {
            device_id: row.get("device_id"),
            role: row.get("role"),
            revoked: row.get("revoked"),
            profile_fingerprint: row.get("profile_fingerprint"),
            key_epoch: row.get::<i32, _>("key_epoch") as u32,
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"devices":devices})))
}

async fn capabilities(State(s): State<ApiState>, h: HeaderMap) -> ApiResult<Json<Value>> {
    let p = auth(&s.db, &h).await?;
    let rows=sqlx::query("SELECT c.device_id,c.simulator,c.capabilities,c.updated_at FROM device_capabilities c JOIN devices d ON d.vault_id=c.vault_id AND d.device_id=c.device_id WHERE c.vault_id=$1 AND d.revoked_at IS NULL ORDER BY c.updated_at DESC").bind(p.vault).fetch_all(&s.db).await.map_err(|error| database_unavailable(&error, "api_query"))?;
    Ok(Json(
        json!({"capabilities":rows.into_iter().map(|r|json!({"device_id":r.get::<Uuid,_>("device_id"),"simulator":r.get::<bool,_>("simulator"),"capabilities":r.get::<Value,_>("capabilities")})).collect::<Vec<_>>()}),
    ))
}
#[derive(Deserialize)]
struct CapabilityUpdate {
    simulator: bool,
    capabilities: Value,
}
async fn update_capabilities(
    State(s): State<ApiState>,
    h: HeaderMap,
    Json(x): Json<CapabilityUpdate>,
) -> ApiResult<StatusCode> {
    let p = auth(&s.db, &h).await?;
    if p.role != "gateway" {
        return Err(ApiError(StatusCode::FORBIDDEN, "gateway_required"));
    }
    sqlx::query("INSERT INTO device_capabilities(vault_id,device_id,simulator,capabilities) VALUES($1,$2,$3,$4) ON CONFLICT(vault_id,device_id) DO UPDATE SET simulator=EXCLUDED.simulator,capabilities=EXCLUDED.capabilities,updated_at=now()")
        .bind(p.vault).bind(p.device).bind(x.simulator).bind(x.capabilities).execute(&s.db).await.map_err(|error| database_unavailable(&error, "api_query"))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct Accepted {
    cursor: String,
    duplicate: bool,
}
async fn ingest(
    State(s): State<ApiState>,
    h: HeaderMap,
    Json(e): Json<Envelope>,
) -> ApiResult<Json<Accepted>> {
    let p = auth(&s.db, &h).await?;
    if e.vault_id.0 != p.vault || e.producer_device_id.0 != p.device {
        return Err(ApiError(StatusCode::FORBIDDEN, "producer_not_authorized"));
    }
    e.validate()
        .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid_envelope"))?;
    let digest = e
        .wire_digest()
        .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid_envelope"))?;
    let mut tx =
        s.db.begin()
            .await
            .map_err(|error| database_unavailable(&error, "ingest_begin"))?;
    // Lock first: cursor allocation, duplicate detection and key-epoch cutover
    // are all serialized per vault, so concurrent identical retries resolve to
    // `duplicate:true` rather than a unique-violation conflict.
    let vault_meta = sqlx::query(
        "SELECT key_epoch,profile_fingerprint FROM vaults WHERE vault_id=$1 FOR UPDATE",
    )
    .bind(p.vault)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| database_unavailable(&error, "ingest_vault_lock"))?;
    // Identity lives in the immutable records, which outlive transport-log
    // pruning. A retry stays observable after a key epoch or route changes.
    let existing=sqlx::query("SELECT cursor,cipher_digest FROM encrypted_records WHERE vault_id=$1 AND producer_device_id=$2 AND (producer_sequence=$3 OR envelope_id=$4 OR (command_id IS NOT NULL AND command_id=$5)) LIMIT 1").bind(p.vault).bind(p.device).bind(e.producer_sequence.0 as i64).bind(e.envelope_id.0).bind(e.command_id.map(|v|v.0)).fetch_optional(&mut *tx).await.map_err(|error| database_unavailable(&error, "ingest_duplicate_lookup"))?;
    if let Some(row) = existing {
        if row.get::<Vec<u8>, _>("cipher_digest") == digest {
            let c: i64 = row.get("cursor");
            return Ok(Json(Accepted {
                cursor: c.to_string(),
                duplicate: true,
            }));
        }
        return Err(ApiError(StatusCode::CONFLICT, "idempotency_conflict"));
    }
    let current_epoch = vault_meta.get::<i32, _>("key_epoch") as u32;
    let current_fingerprint: String = vault_meta.get("profile_fingerprint");
    match e.purpose {
        // New carrier commands are only accepted under the active epoch.
        EnvelopePurpose::Command => {
            if current_epoch != e.key_epoch || current_fingerprint != e.profile_fingerprint {
                return Err(ApiError(StatusCode::CONFLICT, "retired_key_epoch"));
            }
        }
        // Events may lag a manual rotation but must name an activated profile.
        EnvelopePurpose::Event => {
            let known: Option<i32> = sqlx::query_scalar("SELECT 1 FROM vault_key_profiles WHERE vault_id=$1 AND key_epoch=$2 AND profile_fingerprint=$3 AND activated_at IS NOT NULL")
                .bind(p.vault)
                .bind(i32::try_from(e.key_epoch).unwrap_or(-1))
                .bind(&e.profile_fingerprint)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| database_unavailable(&error, "ingest_key_profile"))?;
            if known.is_none() {
                return Err(ApiError(StatusCode::CONFLICT, "key_epoch_not_active"));
            }
        }
    }
    if let Some(route) = &e.route {
        let gateway:Option<i32>=sqlx::query_scalar("SELECT 1 FROM devices WHERE vault_id=$1 AND device_id=$2 AND role='gateway' AND revoked_at IS NULL").bind(p.vault).bind(route.gateway_device_id.0).fetch_optional(&mut *tx).await.map_err(|error| database_unavailable(&error, "ingest_route_lookup"))?;
        if gateway.is_none() {
            return Err(ApiError(StatusCode::FORBIDDEN, "gateway_not_authorized"));
        }
    }
    let c: i64 = sqlx::query_scalar(
        "UPDATE vaults SET next_cursor=next_cursor+1 WHERE vault_id=$1 RETURNING next_cursor",
    )
    .bind(p.vault)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| database_unavailable(&error, "ingest_cursor"))?;
    let payload = serde_json::to_value(&e)
        .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid_envelope"))?;
    let purpose = match e.purpose {
        EnvelopePurpose::Event => "event",
        EnvelopePurpose::Command => "command",
    };
    sqlx::query("INSERT INTO event_log(vault_id,cursor,envelope_id,producer_device_id,producer_sequence,purpose,command_id,cipher_digest,envelope) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(p.vault).bind(c).bind(e.envelope_id.0).bind(p.device).bind(e.producer_sequence.0 as i64).bind(purpose).bind(e.command_id.map(|v|v.0)).bind(digest.as_slice()).bind(&payload).execute(&mut *tx).await.map_err(|error| insert_error(error, "ingest_event_insert"))?;
    sqlx::query("INSERT INTO encrypted_records(vault_id,producer_device_id,envelope_id,cursor,producer_sequence,purpose,command_id,cipher_digest,envelope) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(p.vault)
        .bind(p.device)
        .bind(e.envelope_id.0)
        .bind(c)
        .bind(e.producer_sequence.0 as i64)
        .bind(purpose)
        .bind(e.command_id.map(|v| v.0))
        .bind(digest.as_slice())
        .bind(&payload)
        .execute(&mut *tx)
        .await
        .map_err(|error| insert_error(error, "ingest_record_insert"))?;
    if let Some(id) = e.command_id {
        let gateway = e
            .route
            .as_ref()
            .expect("validated route")
            .gateway_device_id
            .0;
        sqlx::query("INSERT INTO commands(vault_id,producer_device_id,command_id,gateway_device_id,cipher_digest,cursor) VALUES($1,$2,$3,$4,$5,$6)").bind(p.vault).bind(p.device).bind(id.0).bind(gateway).bind(digest.as_slice()).bind(c).execute(&mut *tx).await.map_err(|error| insert_error(error, "ingest_command_insert"))?;
    }
    // A lightweight reference only; the envelope stays in the log/records.
    sqlx::query("INSERT INTO outbox_jobs(vault_id,cursor,kind) VALUES($1,$2,'sync')")
        .bind(p.vault)
        .bind(c)
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "ingest_outbox_insert"))?;
    tx.commit()
        .await
        .map_err(|error| database_unavailable(&error, "ingest_commit"))?;
    // Immediate latency hint; the outbox drainer republishes and sockets rescan.
    let _ = s.committed.send((p.vault, c));
    Ok(Json(Accepted {
        cursor: c.to_string(),
        duplicate: false,
    }))
}

#[derive(Deserialize)]
struct Receipt {
    receipt: Value,
}
async fn receipt(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(command_id): Path<Uuid>,
    Json(x): Json<Receipt>,
) -> ApiResult<StatusCode> {
    let p = auth(&s.db, &h).await?;
    if p.role != "gateway" {
        return Err(ApiError(StatusCode::FORBIDDEN, "gateway_required"));
    }
    let ok: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM commands WHERE vault_id=$1 AND command_id=$2 AND gateway_device_id=$3",
    )
    .bind(p.vault)
    .bind(command_id)
    .bind(p.device)
    .fetch_optional(&s.db)
    .await
    .map_err(|error| database_unavailable(&error, "api_query"))?;
    if ok.is_none() {
        return Err(ApiError(StatusCode::FORBIDDEN, "receipt_not_targeted"));
    }
    sqlx::query("INSERT INTO command_receipts(vault_id,command_id,gateway_device_id,receipt) VALUES($1,$2,$3,$4) ON CONFLICT(vault_id,command_id,gateway_device_id) DO UPDATE SET receipt=EXCLUDED.receipt,created_at=now()").bind(p.vault).bind(command_id).bind(p.device).bind(x.receipt).execute(&s.db).await.map_err(|error| database_unavailable(&error, "api_query"))?;
    Ok(StatusCode::NO_CONTENT)
}

/// WebSocket close codes. Clients reconnect and resume on 4429, take a
/// snapshot on 4409, stop on 4401, and upgrade on 4426.
mod close {
    pub const INVALID_HELLO: u16 = 4400;
    pub const REVOKED: u16 = 4401;
    pub const HELLO_TIMEOUT: u16 = 4408;
    pub const RESYNC_REQUIRED: u16 = 4409;
    pub const UNSUPPORTED_VERSION: u16 = 4426;
    pub const BACKPRESSURE: u16 = 4429;
    pub const SERVER_ERROR: u16 = 1011;
}
const WS_PROTOCOL_VERSION: u64 = 1;
const WS_REPLAY_PAGE: i64 = 100;

async fn websocket(
    State(s): State<ApiState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> ApiResult<axum::response::Response> {
    let principal = auth(&s.db, &headers).await?;
    // Subscribe before replay so a commit between replay and live is never missed.
    let receiver = s.committed.subscribe();
    Ok(upgrade
        .max_message_size(1_100_000)
        .max_frame_size(1_100_000)
        .on_upgrade(move |socket| ws_session(socket, s, principal, receiver))
        .into_response())
}

async fn ws_close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: Utf8Bytes::from_static(reason),
        })))
        .await;
}

/// Why a socket stopped; mapped to one close code.
enum WsStop {
    Resync(sync::Resync),
    Backpressure,
    Database,
    Disconnected,
}

impl WsStop {
    async fn close(self, socket: &mut WebSocket) {
        match self {
            Self::Resync(resync) => {
                let _ = socket.send(Message::Text(resync.ws_frame().into())).await;
                ws_close(socket, close::RESYNC_REQUIRED, "resync_required").await;
            }
            Self::Backpressure => ws_close(socket, close::BACKPRESSURE, "backpressure").await,
            Self::Database => ws_close(socket, close::SERVER_ERROR, "database_unavailable").await,
            Self::Disconnected => {}
        }
    }
}

enum HelloError {
    /// The client went away; there is nobody to send a close frame to.
    Disconnected,
    Rejected(u16, &'static str),
}

/// Reads the client hello within the configured timeout. Pings and pongs are
/// ignored; any other frame is a protocol error.
async fn read_hello(socket: &mut WebSocket, timeout: Duration) -> Result<i64, HelloError> {
    use HelloError::Rejected;
    let deadline = tokio::time::Instant::now() + timeout;
    let text = loop {
        match tokio::time::timeout_at(deadline, socket.recv()).await {
            Err(_) => return Err(Rejected(close::HELLO_TIMEOUT, "hello_timeout")),
            Ok(Some(Ok(Message::Text(text)))) => break text,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Some(Ok(Message::Binary(_)))) => {
                return Err(Rejected(close::INVALID_HELLO, "invalid_hello"));
            }
            Ok(Some(Ok(Message::Close(_)) | Err(_)) | None) => {
                return Err(HelloError::Disconnected);
            }
        }
    };
    let hello: Value =
        serde_json::from_str(&text).map_err(|_| Rejected(close::INVALID_HELLO, "invalid_hello"))?;
    match hello.get("protocol_version").and_then(Value::as_u64) {
        Some(WS_PROTOCOL_VERSION) => {}
        Some(_) => {
            return Err(Rejected(
                close::UNSUPPORTED_VERSION,
                "unsupported_protocol_version",
            ));
        }
        None => return Err(Rejected(close::INVALID_HELLO, "invalid_hello")),
    }
    hello
        .get("resume_cursor")
        .and_then(Value::as_str)
        .and_then(|cursor| sync::parse_cursor(cursor).ok())
        .ok_or(Rejected(close::INVALID_HELLO, "invalid_hello"))
}

async fn ws_session(
    mut socket: WebSocket,
    state: ApiState,
    principal: Principal,
    mut notices: tokio::sync::broadcast::Receiver<(Uuid, i64)>,
) {
    let options = Arc::clone(&state.options);
    let mut cursor = match read_hello(&mut socket, options.hello_timeout).await {
        Ok(cursor) => cursor,
        Err(HelloError::Disconnected) => return,
        Err(HelloError::Rejected(code, reason)) => {
            return ws_close(&mut socket, code, reason).await;
        }
    };
    let (high_water, replay_floor) = match sync::position(&state.db, principal.vault, cursor).await
    {
        Ok(Ok(marks)) => marks,
        Ok(Err(resync)) => return WsStop::Resync(resync).close(&mut socket).await,
        Err(error) => {
            tracing::warn!(error_kind = %error, "websocket handshake position failed");
            return WsStop::Database.close(&mut socket).await;
        }
    };
    // Negotiation acknowledgment: the server's version, the accepted resume
    // point and current watermarks, then replay frames follow.
    let ready = json!({
        "type": "ready",
        "protocol_version": WS_PROTOCOL_VERSION,
        "resume_cursor": cursor.to_string(),
        "high_water_cursor": high_water.to_string(),
        "replay_floor_cursor": replay_floor.to_string(),
        "frame_types": ["ready", "event", "resync_required"],
    });
    if let Err(stop) = send_frame(&mut socket, ready.to_string(), options.send_timeout).await {
        return stop.close(&mut socket).await;
    }
    if let Err(stop) = send_replay(&mut socket, &state, principal.vault, &mut cursor).await {
        return stop.close(&mut socket).await;
    }
    let mut recheck = tokio::time::interval(Duration::from_secs(20));
    // Broadcast is only a latency hint; committed rows are the durable source.
    let mut durable_replay = tokio::time::interval(options.durable_replay_interval);
    durable_replay.tick().await;
    loop {
        let stop = tokio::select! {
            _ = recheck.tick() => match still_active(&state.db, &principal).await {
                Ok(true) => continue,
                Ok(false) => return ws_close(&mut socket, close::REVOKED, "revoked").await,
                Err(error) => {
                    tracing::warn!(error_kind = %error, "websocket revocation recheck failed");
                    WsStop::Database
                }
            },
            _ = durable_replay.tick() => match send_replay(&mut socket, &state, principal.vault, &mut cursor).await {
                Ok(()) => continue,
                Err(stop) => stop,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => {
                    return ws_close(&mut socket, close::INVALID_HELLO, "unexpected_client_frame").await;
                }
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
            },
            notice = notices.recv() => match notice {
                // Hints at or below the delivered cursor are already satisfied.
                Ok((vault, hinted)) if vault == principal.vault && hinted > cursor => {
                    match send_replay(&mut socket, &state, vault, &mut cursor).await {
                        Ok(()) => continue,
                        Err(stop) => stop,
                    }
                }
                Ok(_) => continue,
                // Missed hints are recovered from committed rows, not by
                // disconnecting every lagging socket at once.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    match send_replay(&mut socket, &state, principal.vault, &mut cursor).await {
                        Ok(()) => continue,
                        Err(stop) => stop,
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        };
        return stop.close(&mut socket).await;
    }
}
async fn still_active(db: &PgPool, p: &Principal) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, i32>("SELECT 1 FROM devices d JOIN device_credentials c ON c.vault_id=d.vault_id AND c.device_id=d.device_id WHERE d.vault_id=$1 AND d.device_id=$2 AND d.revoked_at IS NULL AND c.revoked_at IS NULL")
        .bind(p.vault).bind(p.device).fetch_optional(db).await?.is_some())
}
async fn send_frame(
    socket: &mut WebSocket,
    frame: String,
    timeout: Duration,
) -> Result<(), WsStop> {
    match tokio::time::timeout(timeout, socket.send(Message::Text(frame.into()))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(WsStop::Disconnected),
        Err(_) => Err(WsStop::Backpressure),
    }
}
/// Sends every retained committed row after `cursor`, page by page, from the
/// durable log. Any retention or rollback mismatch becomes a typed resync.
async fn send_replay(
    socket: &mut WebSocket,
    state: &ApiState,
    vault: Uuid,
    cursor: &mut i64,
) -> Result<(), WsStop> {
    loop {
        let rows = match sync::replay_page(&state.db, vault, *cursor, WS_REPLAY_PAGE).await {
            Ok(sync::ReplayPage::Rows { rows, .. }) => rows,
            Ok(sync::ReplayPage::Resync(resync)) => return Err(WsStop::Resync(resync)),
            Err(error) => {
                tracing::warn!(error_kind = %error, "websocket replay failed");
                return Err(WsStop::Database);
            }
        };
        if rows.is_empty() {
            return Ok(());
        }
        for record in rows {
            let next = record.cursor;
            let frame = serde_json::to_string(&sync::EventFrame::new(record))
                .map_err(|_| WsStop::Database)?;
            send_frame(socket, frame, state.options.send_timeout).await?;
            *cursor = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_tokens_are_canonical_256_bit_base64url() {
        let token = challenge_token();
        assert_eq!(token.len(), 43);
        assert!(decode_challenge(&token).is_some());
    }

    #[test]
    fn profile_requires_matching_vault_epoch_and_fingerprint() {
        let vault = Uuid::new_v4();
        let salt: Vec<Value> = (0_u8..16).map(|byte| json!(byte)).collect();
        let mut digest = Sha256::new();
        digest.update(b"openpush-key-profile-v1\0");
        digest.update(1_u16.to_be_bytes());
        for byte in 0_u8..16 {
            digest.update([byte]);
        }
        digest.update(vault.as_bytes());
        digest.update(3_u32.to_be_bytes());
        let fingerprint = hex::encode(digest.finalize());
        let profile = json!({"crypto_suite":1,"salt":salt,"vault_id":vault,"key_epoch":3});
        assert_eq!(validate_profile(&profile, &fingerprint, 3), Ok(vault));
        assert!(validate_profile(&profile, &fingerprint, 2).is_err());
    }
}
