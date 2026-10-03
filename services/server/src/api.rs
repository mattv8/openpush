mod attachments;
mod key_profiles;
mod sync;

pub use sync::{DrainStats, TransportOptions, compact_records, drain_outbox, prune_replay_log};

use axum::{
    Json, Router,
    extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post, put},
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use peppy_domain::{DeviceId, VaultId};
use peppy_protocol::{
    Envelope, EnvelopePurpose, pairing_key_digest, pairing_proof_message, pairing_sas,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime},
};
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
    pairing_admissions: Arc<tokio::sync::Mutex<HashMap<[u8; 32], PairingAdmission>>>,
}
struct PairingAdmission {
    window: SystemTime,
    count: u8,
}

/// Capacity of the in-process commit-hint channel shared by all sockets.
const HINT_CAPACITY: usize = 1024;

/// Builds the API with transport policy from the environment
/// (`PEPPY_REPLAY_RETENTION_DAYS`, default 30) and starts the bounded
/// in-process outbox/retention maintenance task.
pub fn router(db: PgPool) -> Router {
    let config = Config::from_env().ok();
    let mut options = TransportOptions::default();
    if let Some(config) = &config {
        options.replay_retention = config.replay_retention;
    }
    build_router(db, config, options, None)
}

/// Same as [`router`] with explicit transport timing/retention policy.
pub fn router_with_options(db: PgPool, options: TransportOptions) -> Router {
    build_router(db, Config::from_env().ok(), options, None)
}

/// Testable router variant with an explicit, fixed relay base URL.
pub fn router_with_options_and_relay(
    db: PgPool,
    options: TransportOptions,
    relay_url: url::Url,
) -> Router {
    build_router(db, None, options, Some(relay_url))
}

fn build_router(
    db: PgPool,
    config: Option<Config>,
    options: TransportOptions,
    explicit_relay_url: Option<url::Url>,
) -> Router {
    let (committed, _) = tokio::sync::broadcast::channel(HINT_CAPACITY);
    let storage = config
        .as_ref()
        .and_then(|config| config.s3.as_ref().map(Storage::new));
    sync::spawn_maintenance(db.clone(), &committed, storage.clone(), options.clone());
    if let Some(relay_url) =
        explicit_relay_url.or_else(|| config.as_ref().and_then(|config| config.relay_url.clone()))
    {
        spawn_wake_maintenance(db.clone(), relay_url);
    }
    let vault_attachment_quota_bytes = config
        .as_ref()
        .map(|config| config.vault_attachment_quota_bytes)
        .unwrap_or(512 * 1024 * 1024);
    Router::new()
        .route("/v1/vault", get(vault_header))
        .route("/v1/pairing", post(create_pairing))
        .route("/v1/pairing/intents", post(create_pairing_intent))
        .route(
            "/v1/pairing/intents/{intent_token}/claim",
            post(claim_pairing_intent),
        )
        .route(
            "/v1/pairing/intents/{intent_token}/approve",
            post(approve_pairing_intent),
        )
        .route(
            "/v1/pairing/intents/{intent_token}",
            get(pairing_intent_status),
        )
        .route(
            "/v1/pairing/intents/{intent_token}/challenge",
            post(retrieve_pairing_challenge),
        )
        .route("/v1/pairing/consume", post(consume_pairing))
        .route("/v1/devices/{device_id}/revoke", post(revoke_device))
        .route("/v1/devices/self/wake-route", put(register_wake_route))
        .route("/v1/devices", get(devices))
        .route(
            "/v1/capabilities",
            get(capabilities).post(update_capabilities),
        )
        .route("/v1/events", post(ingest))
        .route(
            "/v1/compaction/capability",
            post(register_compaction_capability),
        )
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
        .route("/v1/vault", delete(delete_vault))
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
            get(attachments::download_attachment).delete(attachments::release_attachment),
        )
        .route(
            "/v1/attachments/{attachment_id}/references",
            post(attachments::register_attachment_references),
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
            pairing_admissions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
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
    token_digest: Vec<u8>,
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
    let row = sqlx::query("SELECT c.vault_id,c.device_id,d.role,c.token_digest FROM device_credentials c JOIN devices d ON d.vault_id=c.vault_id AND d.device_id=c.device_id WHERE c.token_digest=$1 AND c.revoked_at IS NULL AND d.revoked_at IS NULL")
        .bind(digest.as_slice()).fetch_optional(db).await.map_err(|error| database_unavailable(&error, "api_query"))?.ok_or(ApiError(StatusCode::UNAUTHORIZED,"invalid_bearer"))?;
    Ok(Principal {
        vault: row.get("vault_id"),
        device: row.get("device_id"),
        role: row.get("role"),
        token_digest: row.get("token_digest"),
    })
}

/// Re-check the exact bearer credential after serializing on the vault row.
/// The initial HTTP authentication can otherwise become stale while waiting for
/// a concurrent revocation/delete transaction to release that lock.
async fn revalidate_principal(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    principal: &Principal,
) -> ApiResult<String> {
    sqlx::query_scalar("SELECT d.role FROM device_credentials c JOIN devices d ON d.vault_id=c.vault_id AND d.device_id=c.device_id WHERE c.token_digest=$1 AND c.vault_id=$2 AND c.device_id=$3 AND c.revoked_at IS NULL AND d.revoked_at IS NULL")
        .bind(&principal.token_digest)
        .bind(principal.vault)
        .bind(principal.device)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|error| database_unavailable(&error, "principal_revalidate"))?
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "invalid_bearer"))
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
    digest.update(b"peppy-key-profile-v1\0");
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

const PAIRING_INTENT_TTL_SECONDS: i64 = 300;
const PAIRING_ADMISSION_WINDOW: Duration = Duration::from_secs(60);
const PAIRING_ADMISSION_LIMIT: u8 = 20;
const PAIRING_ADMISSION_CAP: usize = 4096;

async fn pairing_admission(s: &ApiState, token: &[u8; 32]) -> ApiResult<()> {
    let now = SystemTime::now();
    let mut admissions = s.pairing_admissions.lock().await;
    admissions.retain(|_, admission| {
        now.duration_since(admission.window).unwrap_or_default() < PAIRING_ADMISSION_WINDOW
    });
    if admissions.len() >= PAIRING_ADMISSION_CAP && !admissions.contains_key(token) {
        // Token digests are supplied by unauthenticated callers. Evicting the
        // oldest active bucket preserves the per-token limit without allowing a
        // stream of random tokens to deny admission to every new pairing flow.
        if let Some(oldest) = admissions
            .iter()
            .min_by_key(|(_, admission)| admission.window)
            .map(|(token, _)| *token)
        {
            admissions.remove(&oldest);
        }
    }
    let admission = admissions.entry(*token).or_insert(PairingAdmission {
        window: now,
        count: 0,
    });
    if now.duration_since(admission.window).unwrap_or_default() >= PAIRING_ADMISSION_WINDOW {
        *admission = PairingAdmission {
            window: now,
            count: 0,
        };
    }
    if admission.count >= PAIRING_ADMISSION_LIMIT {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "pairing_admission_limited",
        ));
    }
    admission.count += 1;
    Ok(())
}

fn pairing_origin(value: &str) -> bool {
    value.parse::<url::Url>().ok().is_some_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none()
    })
}
fn key_digest(public_key: &Value) -> Option<String> {
    Some(pairing_key_digest(verifying_key(public_key)?.as_bytes()))
}

#[derive(Deserialize)]
struct PairingIntentRequest {
    https_origin: String,
}
#[derive(Serialize)]
struct PairingIntentResponse {
    https_origin: String,
    intent_token: String,
    expires_in_seconds: i64,
}
async fn create_pairing_intent(
    State(s): State<ApiState>,
    h: HeaderMap,
    Json(x): Json<PairingIntentRequest>,
) -> ApiResult<Json<PairingIntentResponse>> {
    let p = auth(&s.db, &h).await?;
    owner(&p)?;
    if !pairing_origin(&x.https_origin) {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_pairing_origin",
        ));
    }
    let token = challenge_token();
    sqlx::query("INSERT INTO pairing_intents(intent_digest,vault_id,origin,created_by_device_id,expires_at) VALUES($1,$2,$3,$4,now()+($5 * interval '1 second'))")
        .bind(Sha256::digest(token.as_bytes()).as_slice()).bind(p.vault).bind(&x.https_origin).bind(p.device).bind(PAIRING_INTENT_TTL_SECONDS)
        .execute(&s.db).await.map_err(|error| unique_conflict(error, "pairing_intent_exists", "pairing_intent_insert"))?;
    Ok(Json(PairingIntentResponse {
        https_origin: x.https_origin,
        intent_token: token,
        expires_in_seconds: PAIRING_INTENT_TTL_SECONDS,
    }))
}

#[derive(Deserialize)]
struct PairingIntentClaim {
    device_id: Uuid,
    public_key: Value,
    requested_role: String,
}
#[derive(Serialize)]
struct PairingIntentClaimResponse {
    key_digest: String,
    sas: String,
    claim_secret: String,
    expires_in_seconds: i64,
}
async fn claim_pairing_intent(
    State(s): State<ApiState>,
    Path(intent_token): Path<String>,
    Json(x): Json<PairingIntentClaim>,
) -> ApiResult<Json<PairingIntentClaimResponse>> {
    let digest = decode_challenge(&intent_token)
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "invalid_pairing_intent"))?;
    pairing_admission(&s, &digest).await?;
    if x.device_id.is_nil() || !matches!(x.requested_role.as_str(), "device" | "gateway") {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_pairing_claim",
        ));
    }
    let key_digest = key_digest(&x.public_key).ok_or(ApiError(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_public_key",
    ))?;
    let mut tx =
        s.db.begin()
            .await
            .map_err(|error| database_unavailable(&error, "pairing_intent_claim_begin"))?;
    let row = sqlx::query("SELECT claimed_device_id,claimed_key_digest,expires_at>now() valid FROM pairing_intents WHERE intent_digest=$1 FOR UPDATE")
        .bind(Sha256::digest(intent_token.as_bytes()).as_slice()).fetch_optional(&mut *tx).await.map_err(|error| database_unavailable(&error, "pairing_intent_claim_lookup"))?
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "pairing_intent_invalid_or_expired"))?;
    if !row.get::<bool, _>("valid") {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "pairing_intent_invalid_or_expired",
        ));
    }
    let claim_secret = challenge_token();
    if let Some(existing) = row.get::<Option<Uuid>, _>("claimed_device_id") {
        if existing != x.device_id
            || row
                .get::<Option<String>, _>("claimed_key_digest")
                .as_deref()
                != Some(&key_digest)
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "pairing_intent_already_claimed",
            ));
        }
        return Err(ApiError(StatusCode::CONFLICT, "pairing_intent_claimed"));
    } else {
        sqlx::query("UPDATE pairing_intents SET claimed_at=now(),claimed_device_id=$2,claimed_public_key=$3,claimed_key_digest=$4,claim_secret_digest=$5,requested_role=$6 WHERE intent_digest=$1")
            .bind(Sha256::digest(intent_token.as_bytes()).as_slice()).bind(x.device_id).bind(x.public_key).bind(&key_digest).bind(Sha256::digest(claim_secret.as_bytes()).as_slice()).bind(&x.requested_role).execute(&mut *tx).await.map_err(|error| database_unavailable(&error, "pairing_intent_claim_update"))?;
    }
    tx.commit()
        .await
        .map_err(|error| database_unavailable(&error, "pairing_intent_claim_commit"))?;
    let _ = digest; // decoding enforces canonical 256-bit token before the database lookup.
    Ok(Json(PairingIntentClaimResponse {
        key_digest: key_digest.clone(),
        sas: pairing_sas(&intent_token, &key_digest, DeviceId(x.device_id)),
        claim_secret,
        expires_in_seconds: PAIRING_INTENT_TTL_SECONDS,
    }))
}

#[derive(Deserialize)]
struct PairingIntentApproval {
    key_digest: String,
    profile_fingerprint: String,
    key_epoch: u32,
}
async fn approve_pairing_intent(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(intent_token): Path<String>,
    Json(x): Json<PairingIntentApproval>,
) -> ApiResult<Json<PairResponse>> {
    let p = auth(&s.db, &h).await?;
    owner(&p)?;
    if !profile_ok(&x.profile_fingerprint)
        || x.key_epoch == 0
        || x.key_digest.len() != 64
        || !x.key_digest.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_pairing_approval",
        ));
    }
    let mut tx =
        s.db.begin()
            .await
            .map_err(|error| database_unavailable(&error, "pairing_intent_approve_begin"))?;
    // Keep the vault lock before the intent lock, matching other roster-changing
    // pairing operations and preventing lock-order inversions.
    let vault_row = sqlx::query(
        "SELECT key_epoch,profile_fingerprint FROM vaults WHERE vault_id=$1 FOR UPDATE",
    )
    .bind(p.vault)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| database_unavailable(&error, "pairing_intent_vault_lock"))?;
    if revalidate_principal(&mut tx, &p).await? != "owner" {
        return Err(ApiError(StatusCode::FORBIDDEN, "owner_required"));
    }
    let row = sqlx::query("SELECT vault_id,origin,claimed_device_id,claimed_public_key,claimed_key_digest,requested_role FROM pairing_intents WHERE intent_digest=$1 AND vault_id=$2 AND expires_at>now() AND approved_at IS NULL FOR UPDATE")
        .bind(Sha256::digest(intent_token.as_bytes()).as_slice()).bind(p.vault).fetch_optional(&mut *tx).await.map_err(|error| database_unavailable(&error, "pairing_intent_approve_lookup"))?
        .ok_or(ApiError(StatusCode::CONFLICT, "pairing_intent_not_claimed_or_expired"))?;
    let device: Uuid = row
        .get::<Option<Uuid>, _>("claimed_device_id")
        .ok_or(ApiError(
            StatusCode::CONFLICT,
            "pairing_intent_not_claimed_or_expired",
        ))?;
    let actual: String = row
        .get::<Option<String>, _>("claimed_key_digest")
        .ok_or(ApiError(
            StatusCode::CONFLICT,
            "pairing_intent_not_claimed_or_expired",
        ))?;
    if actual != x.key_digest {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "pairing_key_digest_mismatch",
        ));
    }
    if vault_row.get::<i32, _>("key_epoch") as u32 != x.key_epoch
        || vault_row.get::<String, _>("profile_fingerprint") != x.profile_fingerprint
    {
        return Err(ApiError(StatusCode::CONFLICT, "profile_or_epoch_mismatch"));
    }
    let challenge = challenge_token();
    let public_key: Value = row
        .get::<Option<Value>, _>("claimed_public_key")
        .expect("claimed key accompanies claimed device");
    let role: String = row
        .get::<Option<String>, _>("requested_role")
        .expect("claimed role accompanies claimed device");
    sqlx::query("INSERT INTO pairing_challenges(challenge_digest,vault_id,requested_device_id,requested_public_key,approved_by_device_id,profile_fingerprint,key_epoch,requested_role,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,now()+interval '120 seconds')")
        .bind(Sha256::digest(challenge.as_bytes()).as_slice()).bind(p.vault).bind(device).bind(public_key).bind(p.device).bind(&x.profile_fingerprint).bind(x.key_epoch as i32).bind(&role).execute(&mut *tx).await.map_err(|error| unique_conflict(error, "pairing_exists", "pairing_approval_insert"))?;
    sqlx::query("UPDATE pairing_intents SET approved_at=now(),challenge_token=$2,challenge_digest=$3 WHERE intent_digest=$1")
        .bind(Sha256::digest(intent_token.as_bytes()).as_slice())
        .bind(&challenge)
        .bind(Sha256::digest(challenge.as_bytes()).as_slice())
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "pairing_intent_approve_mark"))?;
    tx.commit()
        .await
        .map_err(|error| database_unavailable(&error, "pairing_intent_approve_commit"))?;
    Ok(Json(PairResponse {
        challenge_token: challenge,
        expires_in_seconds: 120,
        vault_id: p.vault,
        key_epoch: x.key_epoch,
        profile_fingerprint: x.profile_fingerprint,
        requested_role: role,
    }))
}

#[derive(Serialize)]
struct PairingIntentStatus {
    claimed: bool,
    approved: bool,
    device_id: Option<Uuid>,
    key_digest: Option<String>,
    requested_role: Option<String>,
    sas: Option<String>,
    expires_in_seconds: i64,
}
async fn pairing_intent_status(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(intent_token): Path<String>,
) -> ApiResult<Json<PairingIntentStatus>> {
    let p = auth(&s.db, &h).await?;
    owner(&p)?;
    let row = sqlx::query("SELECT claimed_device_id,claimed_key_digest,requested_role,approved_at IS NOT NULL approved,GREATEST(0,extract(epoch FROM expires_at-now())::bigint) remaining FROM pairing_intents WHERE intent_digest=$1 AND vault_id=$2 AND expires_at>now()")
        .bind(Sha256::digest(intent_token.as_bytes()).as_slice()).bind(p.vault).fetch_optional(&s.db).await.map_err(|error| database_unavailable(&error, "pairing_intent_status"))?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "pairing_intent_not_found"))?;
    let device = row.get::<Option<Uuid>, _>("claimed_device_id");
    let digest = row.get::<Option<String>, _>("claimed_key_digest");
    Ok(Json(PairingIntentStatus {
        claimed: device.is_some(),
        approved: row.get("approved"),
        device_id: device,
        key_digest: digest.clone(),
        requested_role: row.get("requested_role"),
        sas: device
            .zip(digest)
            .map(|(device, digest)| pairing_sas(&intent_token, &digest, DeviceId(device))),
        expires_in_seconds: row.get("remaining"),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChallengeRetrieval {
    device_id: Uuid,
    key_digest: String,
    claim_secret: String,
}
async fn retrieve_pairing_challenge(
    State(s): State<ApiState>,
    Path(intent_token): Path<String>,
    Json(x): Json<ChallengeRetrieval>,
) -> ApiResult<Json<PairResponse>> {
    let intent_digest = decode_challenge(&intent_token).ok_or(ApiError(
        StatusCode::UNAUTHORIZED,
        "invalid_pairing_claim_secret",
    ))?;
    if decode_challenge(&x.claim_secret).is_none() {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid_pairing_claim_secret",
        ));
    }
    pairing_admission(&s, &intent_digest).await?;
    let row = sqlx::query("SELECT i.vault_id,i.challenge_token,i.requested_role,c.expires_at>now() valid,GREATEST(0,extract(epoch FROM c.expires_at-now())::bigint) remaining,c.profile_fingerprint,c.key_epoch FROM pairing_intents i JOIN pairing_challenges c ON c.challenge_digest=i.challenge_digest AND c.consumed_at IS NULL WHERE i.intent_digest=$1 AND i.approved_at IS NOT NULL AND i.claimed_device_id=$2 AND i.claimed_key_digest=$3 AND i.claim_secret_digest=$4")
        .bind(Sha256::digest(intent_token.as_bytes()).as_slice()).bind(x.device_id).bind(&x.key_digest).bind(Sha256::digest(x.claim_secret.as_bytes()).as_slice()).fetch_optional(&s.db).await.map_err(|error| database_unavailable(&error, "pairing_challenge_retrieve"))?
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "pairing_challenge_unavailable"))?;
    if !row.get::<bool, _>("valid") {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "pairing_challenge_unavailable",
        ));
    }
    // Only a digest is stored for the challenge, so retain the opaque bearer on the
    // intent after approval; this endpoint returns it only to the claim-secret holder.
    let token: String = row.get("challenge_token");
    Ok(Json(PairResponse {
        challenge_token: token,
        expires_in_seconds: row.get::<i64, _>("remaining") as u16,
        vault_id: row.get("vault_id"),
        key_epoch: row.get::<i32, _>("key_epoch") as u32,
        profile_fingerprint: row.get("profile_fingerprint"),
        requested_role: row.get("requested_role"),
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
    pairing_admission(&s, &challenge).await?;
    let d = Sha256::digest(x.challenge_token.as_bytes());
    let mut tx =
        s.db.begin()
            .await
            .map_err(|error| database_unavailable(&error, "pairing_consume_begin"))?;
    // Discover the vault without locking the challenge, then take the vault lock
    // before the challenge lock. This matches approval and all roster changes.
    let challenge_vault: Uuid = sqlx::query_scalar("SELECT vault_id FROM pairing_challenges WHERE challenge_digest=$1 AND consumed_at IS NULL AND expires_at>now() AND requested_device_id=$2 AND requested_public_key=$3 AND profile_fingerprint=$4 AND key_epoch=$5")
        .bind(d.as_slice()).bind(x.device_id).bind(&x.public_key).bind(&x.profile_fingerprint).bind(x.key_epoch as i32).fetch_optional(&mut *tx).await.map_err(|error| database_unavailable(&error, "pairing_consume_vault_lookup"))?.ok_or(ApiError(StatusCode::UNAUTHORIZED,"challenge_invalid_or_consumed"))?;
    sqlx::query("SELECT 1 FROM vaults WHERE vault_id=$1 FOR UPDATE")
        .bind(challenge_vault)
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "pairing_consume_vault_lock"))?;
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
    // The challenge is consumed; retain only its digest for cleanup, not either
    // bearer secret on the intent row.
    sqlx::query("UPDATE pairing_intents SET claim_secret_digest=NULL,challenge_token=NULL WHERE challenge_digest=$1")
        .bind(d.as_slice())
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "pairing_consume_clear_secrets"))?;
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
    if device_id != p.device {
        owner(&p)?;
    }
    let mut tx =
        s.db.begin()
            .await
            .map_err(|error| database_unavailable(&error, "revoke_begin"))?;
    // Serialize every roster change with pairing/compaction on the vault row.
    sqlx::query("SELECT 1 FROM vaults WHERE vault_id=$1 FOR UPDATE")
        .bind(p.vault)
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "revoke_vault_lock"))?;
    let caller_role = revalidate_principal(&mut tx, &p).await?;
    if device_id != p.device && caller_role != "owner" {
        return Err(ApiError(StatusCode::FORBIDDEN, "owner_required"));
    }
    let target = sqlx::query("SELECT role,revoked_at IS NULL active FROM devices WHERE vault_id=$1 AND device_id=$2 FOR UPDATE").bind(p.vault).bind(device_id).fetch_optional(&mut *tx).await.map_err(|error| database_unavailable(&error, "revoke_target_lookup"))?.ok_or(ApiError(StatusCode::NOT_FOUND, "device_not_found"))?;
    if !target.get::<bool, _>("active") {
        return Err(ApiError(StatusCode::CONFLICT, "device_already_revoked"));
    }
    if target.get::<String, _>("role") == "owner" {
        let owners: i64 = sqlx::query_scalar("SELECT count(*) FROM devices WHERE vault_id=$1 AND role='owner' AND revoked_at IS NULL").bind(p.vault).fetch_one(&mut *tx).await.map_err(|error| database_unavailable(&error, "revoke_owner_count"))?;
        if owners <= 1 {
            return Err(ApiError(StatusCode::CONFLICT, "last_active_owner"));
        }
    }
    sqlx::query("UPDATE devices SET revoked_at=now() WHERE vault_id=$1 AND device_id=$2")
        .bind(p.vault)
        .bind(device_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "revoke_device"))?;
    sqlx::query(
        "UPDATE device_credentials SET revoked_at=now() WHERE vault_id=$1 AND device_id=$2",
    )
    .bind(p.vault)
    .bind(device_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| database_unavailable(&error, "revoke_credential"))?;
    sqlx::query("UPDATE device_wake_routes SET revoked_at=now() WHERE vault_id=$1 AND device_id=$2 AND revoked_at IS NULL").bind(p.vault).bind(device_id).execute(&mut *tx).await.map_err(|error| database_unavailable(&error, "revoke_wake_route"))?;
    sqlx::query("DELETE FROM device_wake_jobs WHERE vault_id=$1 AND device_id=$2")
        .bind(p.vault)
        .bind(device_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "revoke_wake_job"))?;
    tx.commit()
        .await
        .map_err(|error| database_unavailable(&error, "revoke_commit"))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WakeRouteRequest {
    route_id: Uuid,
    wake_credential: String,
}
async fn register_wake_route(
    State(s): State<ApiState>,
    h: HeaderMap,
    Json(x): Json<WakeRouteRequest>,
) -> ApiResult<StatusCode> {
    let p = auth(&s.db, &h).await?;
    if x.route_id.is_nil()
        || x.wake_credential.len() != 64
        || !x.wake_credential.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_wake_route",
        ));
    }
    sqlx::query("INSERT INTO device_wake_routes(vault_id,device_id,route_id,wake_credential) VALUES($1,$2,$3,$4) ON CONFLICT(vault_id,device_id) DO UPDATE SET route_id=EXCLUDED.route_id,wake_credential=EXCLUDED.wake_credential,revoked_at=NULL,created_at=now(),generation=device_wake_routes.generation+1")
        .bind(p.vault).bind(p.device).bind(x.route_id).bind(x.wake_credential).execute(&s.db).await.map_err(|error| database_unavailable(&error, "wake_route_register"))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultDeleteRequest {
    vault_id: Uuid,
}
async fn delete_vault(
    State(s): State<ApiState>,
    h: HeaderMap,
    Json(x): Json<VaultDeleteRequest>,
) -> ApiResult<StatusCode> {
    let p = auth(&s.db, &h).await?;
    owner(&p)?;
    if x.vault_id != p.vault {
        return Err(ApiError(StatusCode::CONFLICT, "vault_id_mismatch"));
    }
    let mut tx =
        s.db.begin()
            .await
            .map_err(|error| database_unavailable(&error, "vault_delete_begin"))?;
    sqlx::query("SELECT 1 FROM vaults WHERE vault_id=$1 FOR UPDATE")
        .bind(p.vault)
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "vault_delete_lock"))?;
    if revalidate_principal(&mut tx, &p).await? != "owner" {
        return Err(ApiError(StatusCode::FORBIDDEN, "owner_required"));
    }
    // Queue every private, public, and reserved object before making public copies unavailable.
    sqlx::query("INSERT INTO storage_deletions(object_key,vault_id,reason) SELECT object_key,$1,'vault_deleted' FROM (SELECT object_key FROM upload_reservations WHERE vault_id=$1 UNION SELECT object_key FROM attachments WHERE vault_id=$1 UNION SELECT object_key FROM public_attachment_copies WHERE vault_id=$1) keys ON CONFLICT(object_key) DO UPDATE SET reason='vault_deleted',not_before=GREATEST(storage_deletions.not_before,now())")
        .bind(p.vault).execute(&mut *tx).await.map_err(|error| database_unavailable(&error, "vault_delete_queue_objects"))?;
    sqlx::query("SET LOCAL peppy.compaction = 'on'")
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "vault_delete_compaction_guard"))?;
    sqlx::query("SET LOCAL peppy.vault_delete = 'on'")
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "vault_delete_guard"))?;
    for table in [
        "device_wake_jobs",
        "device_wake_routes",
        "command_receipts",
        "commands",
        "device_cursors",
        "device_capabilities",
        "attachment_record_references",
        "record_supersessions",
        "record_compaction",
        "compacted_records",
        "event_log",
        "encrypted_records",
        "public_attachment_copies",
        "attachments",
        "upload_reservations",
        "outbox_jobs",
        "pairing_challenges",
        "pairing_intents",
        "vault_key_profiles",
        "device_credentials",
        "devices",
    ] {
        let query = format!("DELETE FROM {table} WHERE vault_id=$1");
        sqlx::query(&query)
            .bind(p.vault)
            .execute(&mut *tx)
            .await
            .map_err(|error| database_unavailable(&error, "vault_delete_dependents"))?;
    }
    sqlx::query("DELETE FROM vaults WHERE vault_id=$1")
        .bind(p.vault)
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "vault_delete_row"))?;
    tx.commit()
        .await
        .map_err(|error| database_unavailable(&error, "vault_delete_commit"))?;
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
/// Explicit opt-in is required from every non-revoked consumer before this
/// vault can delete immutable snapshot rows.
async fn register_compaction_capability(
    State(s): State<ApiState>,
    h: HeaderMap,
) -> ApiResult<StatusCode> {
    let p = auth(&s.db, &h).await?;
    sqlx::query("UPDATE devices SET compaction_generation_fence=TRUE WHERE vault_id=$1 AND device_id=$2 AND revoked_at IS NULL")
        .bind(p.vault).bind(p.device).execute(&s.db).await
        .map_err(|error| database_unavailable(&error, "compaction_capability_register"))?;
    Ok(StatusCode::NO_CONTENT)
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
    let compacted=sqlx::query("SELECT original_cursor,cipher_digest FROM compacted_records WHERE vault_id=$1 AND producer_device_id=$2 AND (producer_sequence=$3 OR envelope_id=$4 OR (command_id IS NOT NULL AND command_id=$5)) LIMIT 1").bind(p.vault).bind(p.device).bind(e.producer_sequence.0 as i64).bind(e.envelope_id.0).bind(e.command_id.map(|v|v.0)).fetch_optional(&mut *tx).await.map_err(|error| database_unavailable(&error, "ingest_compacted_duplicate_lookup"))?;
    if let Some(row) = compacted {
        if row.get::<Vec<u8>, _>("cipher_digest") == digest {
            return Ok(Json(Accepted {
                cursor: row.get::<i64, _>("original_cursor").to_string(),
                duplicate: true,
            }));
        }
        return Err(ApiError(StatusCode::CONFLICT, "idempotency_conflict"));
    }
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
    // Compaction is opaque bookkeeping only. The envelope digest intentionally
    // excludes it so a retry against an older server remains idempotent; the
    // first accepted arrival is consequently authoritative for this metadata.
    if let Some(compaction) = &e.compaction {
        sqlx::query("INSERT INTO record_compaction(vault_id,cursor,compaction_key,terminal) VALUES($1,$2,$3,$4)")
            .bind(p.vault)
            .bind(c)
            .bind(&compaction.key)
            .bind(compaction.terminal)
            .execute(&mut *tx)
            .await
            .map_err(|error| insert_error(error, "ingest_compaction_insert"))?;
        for reference in &compaction.supersedes {
            sqlx::query("INSERT INTO record_supersessions(vault_id,by_cursor,target_producer_device_id,target_producer_sequence) VALUES($1,$2,$3,$4)")
                .bind(p.vault)
                .bind(c)
                .bind(reference.producer_device_id.0)
                .bind(reference.producer_sequence.0 as i64)
                .execute(&mut *tx)
                .await
                .map_err(|error| insert_error(error, "ingest_supersession_insert"))?;
        }
    }
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
    // Wake routes are device-scoped and coalesced; ciphertext remains opaque and
    // no event type (including contacts) is inspected here.
    sqlx::query("INSERT INTO device_wake_jobs(vault_id,device_id,cursor,job_id,opaque_nonce) SELECT r.vault_id,r.device_id,$2,gen_random_uuid(),encode(uuid_send(gen_random_uuid()) || uuid_send(gen_random_uuid()),'base64') FROM device_wake_routes r JOIN devices d ON d.vault_id=r.vault_id AND d.device_id=r.device_id WHERE r.vault_id=$1 AND r.revoked_at IS NULL AND d.revoked_at IS NULL AND r.device_id<>$3 ON CONFLICT(vault_id,device_id) DO UPDATE SET cursor=GREATEST(device_wake_jobs.cursor,EXCLUDED.cursor),job_id=CASE WHEN device_wake_jobs.cursor < EXCLUDED.cursor THEN EXCLUDED.job_id ELSE device_wake_jobs.job_id END,opaque_nonce=CASE WHEN device_wake_jobs.cursor < EXCLUDED.cursor THEN EXCLUDED.opaque_nonce ELSE device_wake_jobs.opaque_nonce END,attempts=CASE WHEN device_wake_jobs.cursor < EXCLUDED.cursor THEN 0 ELSE device_wake_jobs.attempts END,lease_token=CASE WHEN device_wake_jobs.cursor < EXCLUDED.cursor THEN NULL ELSE device_wake_jobs.lease_token END,lease_until=CASE WHEN device_wake_jobs.cursor < EXCLUDED.cursor THEN NULL ELSE device_wake_jobs.lease_until END,available_at=CASE WHEN device_wake_jobs.cursor < EXCLUDED.cursor THEN now() WHEN device_wake_jobs.lease_until IS NULL OR device_wake_jobs.lease_until <= now() THEN LEAST(device_wake_jobs.available_at,now()) ELSE device_wake_jobs.available_at END")
        .bind(p.vault).bind(c).bind(p.device).execute(&mut *tx).await.map_err(|error| database_unavailable(&error, "ingest_wake_enqueue"))?;
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

/// Removes terminal pairing state while clearing bearer secrets before deleting
/// their rows. Active approved challenges outlive their original QR intent TTL.
pub async fn prune_expired_pairing_intents(db: &PgPool) -> Result<u64, sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE pairing_intents SET claim_secret_digest=NULL,challenge_token=NULL WHERE (approved_at IS NULL AND expires_at <= now()) OR (challenge_digest IS NOT NULL AND EXISTS (SELECT 1 FROM pairing_challenges c WHERE c.challenge_digest=pairing_intents.challenge_digest AND (c.consumed_at IS NOT NULL OR c.expires_at <= now())))")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM pairing_challenges WHERE consumed_at IS NOT NULL OR expires_at <= now()",
    )
    .execute(&mut *tx)
    .await?;
    let deleted = sqlx::query("DELETE FROM pairing_intents i WHERE (i.approved_at IS NULL AND i.expires_at <= now()) OR (i.approved_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM pairing_challenges c WHERE c.challenge_digest=i.challenge_digest))")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(deleted)
}

/// The relay endpoint is an operator-fixed origin. This worker sends only the
/// route credential and opaque identifiers; it never handles provider or route
/// management credentials and it is entirely disabled when no relay is set.
const WAKE_BATCH_SIZE: i64 = 16;
const WAKE_LEASE_SECONDS: i64 = 30;
const WAKE_MAX_ATTEMPTS: i32 = 8;
const WAKE_MAX_AGE_HOURS: i64 = 24;

fn relay_wake_url(relay_url: &url::Url, route: Uuid) -> Result<url::Url, url::ParseError> {
    let mut base = relay_url.clone();
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    base.join(&format!("v1/routes/{route}/wake"))
}

fn spawn_wake_maintenance(db: PgPool, relay_url: url::Url) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(error = %error, "wake relay client unavailable");
                return;
            }
        };
        let mut timer = tokio::time::interval(Duration::from_millis(250));
        loop {
            timer.tick().await;
            for _ in 0..WAKE_BATCH_SIZE {
                let lease = Uuid::new_v4();
                let row = sqlx::query("WITH due AS (SELECT j.vault_id,j.device_id FROM device_wake_jobs j JOIN device_wake_routes r ON r.vault_id=j.vault_id AND r.device_id=j.device_id WHERE j.available_at<=now() AND (j.lease_until IS NULL OR j.lease_until<=now()) AND r.revoked_at IS NULL ORDER BY j.available_at LIMIT 1 FOR UPDATE OF j SKIP LOCKED) UPDATE device_wake_jobs j SET lease_token=$1,lease_until=now()+($2 * interval '1 second'),last_attempt_at=now() FROM due JOIN device_wake_routes r ON r.vault_id=due.vault_id AND r.device_id=due.device_id WHERE j.vault_id=due.vault_id AND j.device_id=due.device_id RETURNING j.vault_id,j.device_id,j.cursor,j.job_id,j.opaque_nonce,j.attempts,j.created_at > now()-($3 * interval '1 hour') fresh,r.route_id,r.generation,r.wake_credential")
                    .bind(lease).bind(WAKE_LEASE_SECONDS).bind(WAKE_MAX_AGE_HOURS).fetch_optional(&db).await;
                let Ok(Some(row)) = row else {
                    break;
                };
                let vault: Uuid = row.get("vault_id");
                let device: Uuid = row.get("device_id");
                let cursor: i64 = row.get("cursor");
                let job_id: Uuid = row.get("job_id");
                let nonce: String = row.get("opaque_nonce");
                let attempts: i32 = row.get("attempts");
                let fresh: bool = row.get("fresh");
                let route: Uuid = row.get("route_id");
                let generation: i64 = row.get("generation");
                let credential: String = row.get("wake_credential");
                let url = match relay_wake_url(&relay_url, route) {
                    Ok(url) => url,
                    Err(error) => {
                        tracing::warn!(error = %error, "invalid wake relay URL");
                        break;
                    }
                };
                let response = client.post(url).json(&json!({"wake_credential":credential,"idempotency_id":job_id,"opaque_nonce":nonce})).send().await;
                let status = response.as_ref().ok().map(|value| value.status());
                if matches!(status, Some(StatusCode::ACCEPTED)) {
                    let _ = sqlx::query("DELETE FROM device_wake_jobs WHERE vault_id=$1 AND device_id=$2 AND job_id=$3 AND cursor=$4 AND lease_token=$5").bind(vault).bind(device).bind(job_id).bind(cursor).bind(lease).execute(&db).await;
                } else if matches!(status, Some(StatusCode::UNAUTHORIZED)) {
                    let revoked = sqlx::query("UPDATE device_wake_routes SET revoked_at=now() WHERE vault_id=$1 AND device_id=$2 AND route_id=$3 AND generation=$4 AND revoked_at IS NULL").bind(vault).bind(device).bind(route).bind(generation).execute(&db).await.map(|result| result.rows_affected()).unwrap_or(0);
                    if revoked == 1 {
                        let _ = sqlx::query("DELETE FROM device_wake_jobs WHERE vault_id=$1 AND device_id=$2 AND job_id=$3 AND cursor=$4 AND lease_token=$5").bind(vault).bind(device).bind(job_id).bind(cursor).bind(lease).execute(&db).await;
                    } else {
                        let _ = sqlx::query("UPDATE device_wake_jobs SET lease_token=NULL,lease_until=NULL,available_at=now() WHERE vault_id=$1 AND device_id=$2 AND job_id=$3 AND lease_token=$4").bind(vault).bind(device).bind(job_id).bind(lease).execute(&db).await;
                    }
                } else {
                    let permanent = status.is_some_and(|status| {
                        status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS
                    });
                    let terminal = permanent || attempts + 1 >= WAKE_MAX_ATTEMPTS || !fresh;
                    if terminal {
                        let _ = sqlx::query("DELETE FROM device_wake_jobs WHERE vault_id=$1 AND device_id=$2 AND job_id=$3 AND cursor=$4 AND lease_token=$5").bind(vault).bind(device).bind(job_id).bind(cursor).bind(lease).execute(&db).await;
                        tracing::warn!(status = ?status.map(|value| value.as_u16()), "wake relay delivery abandoned");
                    } else {
                        let delay = 2_i64.pow((attempts.max(0) as u32).min(8));
                        let _ = sqlx::query("UPDATE device_wake_jobs SET attempts=attempts+1,lease_token=NULL,lease_until=NULL,available_at=now()+($4 * interval '1 second') WHERE vault_id=$1 AND device_id=$2 AND job_id=$3 AND lease_token=$5").bind(vault).bind(device).bind(job_id).bind(delay).bind(lease).execute(&db).await;
                    }
                }
            }
        }
    });
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
        digest.update(b"peppy-key-profile-v1\0");
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
