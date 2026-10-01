//! Ciphertext attachments, public image copies, quota admission and the
//! durable object-deletion queue.
//!
//! Object-key safety rules:
//! - Every upload attempt writes a fresh key. A late or failed attempt can only
//!   ever delete its own key, never one referenced by a finalized attachment.
//! - A key that may become garbage is recorded in `storage_deletions` before or
//!   in the same transaction that makes it unreferenced. Upload/public-copy
//!   attempts are recorded as future-dated intents before their PUT and are
//!   claimed when they become referenced.
//! - The worker re-checks references before deleting and retries failures with
//!   backoff; nothing depends on a single inline delete.

use std::path::PathBuf;
use std::time::Duration;

use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use super::{ApiError, ApiResult, ApiState, auth, database_unavailable, insert_error};
use crate::storage::{Storage, StorageError};

const MAX_ATTACHMENT_BYTES: i64 = 64 * 1024 * 1024;
const MAX_PUBLIC_COPY_BYTES: i64 = 10 * 1024 * 1024;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// Grace before an unclaimed attempt intent becomes deletable. It exceeds the
/// body deadline plus the storage PUT deadline, so a live attempt is never reaped.
const ATTEMPT_INTENT_GRACE_SECONDS: i64 = 30 * 60;
/// Maximum deletion retry backoff.
const MAX_DELETE_BACKOFF_SECONDS: i64 = 3_600;
const RESERVATION_TTL_SECONDS: u16 = 3_600;

fn sha256_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    hex::decode(value).ok()
}

fn storage(state: &ApiState) -> ApiResult<&Storage> {
    state.storage.as_ref().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "storage_unavailable",
    ))
}

/// Maps a storage failure: a missing object is a durable typed condition,
/// everything else is transient.
fn storage_failure(error: StorageError, missing: ApiError, operation: &'static str) -> ApiError {
    match error {
        StorageError::NotFound => missing,
        other => {
            tracing::warn!(operation, error_kind = %other, "storage operation failed");
            ApiError(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable")
        }
    }
}

fn upload_slot(state: &ApiState) -> ApiResult<tokio::sync::OwnedSemaphorePermit> {
    state
        .upload_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError(StatusCode::TOO_MANY_REQUESTS, "too_many_uploads"))
}

/// Process-scoped spool directory, so leftovers are attributable and testable.
fn spool_dir() -> PathBuf {
    std::env::temp_dir().join(format!("openpush-spool-{}", std::process::id()))
}

/// A request-body spool file removed on drop, including when the request
/// future is cancelled or any error path returns early.
struct Spool {
    path: PathBuf,
    file: Option<tokio::fs::File>,
}

impl Spool {
    async fn create() -> ApiResult<Self> {
        let unavailable = || ApiError(StatusCode::SERVICE_UNAVAILABLE, "upload_spool_unavailable");
        let directory = spool_dir();
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|_| unavailable())?;
        let path = directory.join(Uuid::new_v4().to_string());
        let file = tokio::fs::File::create(&path)
            .await
            .map_err(|_| unavailable())?;
        Ok(Self {
            path,
            file: Some(file),
        })
    }

    /// Streams `body` into the spool, enforcing `limit` and the upload deadline.
    /// Returns the byte count, SHA-256 and the first `sniff` bytes written.
    async fn fill(
        &mut self,
        body: Body,
        limit: i64,
        too_large: &'static str,
        sniff: usize,
    ) -> ApiResult<(i64, Vec<u8>, Vec<u8>)> {
        let unavailable = || ApiError(StatusCode::SERVICE_UNAVAILABLE, "upload_spool_unavailable");
        let file = self.file.as_mut().ok_or_else(unavailable)?;
        let mut digest = Sha256::new();
        let mut received = 0_i64;
        let mut prefix = Vec::with_capacity(sniff);
        let mut stream = body.into_data_stream();
        let deadline = tokio::time::Instant::now() + UPLOAD_TIMEOUT;
        while let Some(chunk) = tokio::time::timeout_at(deadline, stream.next())
            .await
            .map_err(|_| ApiError(StatusCode::REQUEST_TIMEOUT, "upload_timeout"))?
        {
            let chunk =
                chunk.map_err(|_| ApiError(StatusCode::BAD_REQUEST, "invalid_upload_body"))?;
            received = received
                .checked_add(chunk.len() as i64)
                .filter(|total| *total <= limit)
                .ok_or(ApiError(StatusCode::PAYLOAD_TOO_LARGE, too_large))?;
            if prefix.len() < sniff {
                let take = chunk.len().min(sniff - prefix.len());
                prefix.extend_from_slice(&chunk[..take]);
            }
            digest.update(&chunk);
            file.write_all(&chunk).await.map_err(|_| unavailable())?;
        }
        file.flush().await.map_err(|_| unavailable())?;
        // Close the handle before the storage adapter reopens the path.
        self.file = None;
        Ok((received, digest.finalize().to_vec(), prefix))
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        self.file = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bytes counted against the vault quota: finalized attachments, live
/// reservations and every public copy whose object is not yet purged.
async fn quota_used(tx: &mut Transaction<'_, Postgres>, vault: Uuid) -> ApiResult<i64> {
    sqlx::query_scalar("SELECT (COALESCE((SELECT SUM(ciphertext_bytes) FROM attachments WHERE vault_id=$1),0) + COALESCE((SELECT SUM(declared_bytes) FROM upload_reservations WHERE vault_id=$1 AND finalized_at IS NULL AND deleting_at IS NULL AND expires_at > now()),0) + COALESCE((SELECT SUM(byte_count) FROM public_attachment_copies WHERE vault_id=$1 AND purged_at IS NULL),0))::bigint")
        .bind(vault)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| database_unavailable(&error, "attachment_quota_sum"))
}

async fn lock_vault(tx: &mut Transaction<'_, Postgres>, vault: Uuid) -> ApiResult<()> {
    sqlx::query("SELECT vault_id FROM vaults WHERE vault_id=$1 FOR UPDATE")
        .bind(vault)
        .fetch_one(&mut **tx)
        .await
        .map(|_| ())
        .map_err(|error| database_unavailable(&error, "attachment_quota_lock"))
}

/// Records `key` as an unclaimed attempt that becomes deletable after the grace.
async fn record_intent(db: &PgPool, key: &str, vault: Uuid, reason: &str) -> ApiResult<()> {
    sqlx::query("INSERT INTO storage_deletions(object_key,vault_id,reason,not_before) VALUES($1,$2,$3,now() + ($4::bigint * interval '1 second'))")
        .bind(key)
        .bind(vault)
        .bind(reason)
        .bind(ATTEMPT_INTENT_GRACE_SECONDS)
        .execute(db)
        .await
        .map(|_| ())
        .map_err(|error| database_unavailable(&error, "storage_intent_insert"))
}

const QUEUE_NOW: &str = "INSERT INTO storage_deletions(object_key,vault_id,reason) VALUES($1,$2,$3) ON CONFLICT (object_key) DO UPDATE SET not_before=now()";

/// Makes `key` deletable now. Used for the attempt's own unreferenced key when
/// it cannot be attached; logs rather than failing the already-failed request.
async fn queue_now(db: &PgPool, key: &str, vault: Uuid, reason: &str) {
    if let Err(error) = sqlx::query(QUEUE_NOW)
        .bind(key)
        .bind(vault)
        .bind(reason)
        .execute(db)
        .await
    {
        tracing::warn!(error_kind = %error, "storage deletion enqueue failed; intent grace still applies");
    }
}

/// Claims an attempt intent inside `tx`. Fails if the worker already reaped it.
async fn claim_intent(tx: &mut Transaction<'_, Postgres>, key: &str) -> ApiResult<bool> {
    sqlx::query_scalar::<_, i32>("DELETE FROM storage_deletions WHERE object_key=$1 RETURNING 1")
        .bind(key)
        .fetch_optional(&mut **tx)
        .await
        .map(|row| row.is_some())
        .map_err(|error| database_unavailable(&error, "storage_intent_claim"))
}

#[derive(Deserialize)]
pub(super) struct ReserveAttachment {
    attachment_id: Option<Uuid>,
    declared_ciphertext_bytes: i64,
    declared_ciphertext_sha256: String,
}
#[derive(Serialize)]
pub(super) struct ReservedAttachment {
    attachment_id: Uuid,
    expires_in_seconds: u16,
}

pub(super) async fn reserve_attachment(
    State(s): State<ApiState>,
    h: HeaderMap,
    Json(request): Json<ReserveAttachment>,
) -> ApiResult<Json<ReservedAttachment>> {
    let principal = auth(&s.db, &h).await?;
    storage(&s)?;
    let hash = sha256_hex(&request.declared_ciphertext_sha256).ok_or(ApiError(
        StatusCode::BAD_REQUEST,
        "invalid_ciphertext_sha256",
    ))?;
    if !(1..=MAX_ATTACHMENT_BYTES).contains(&request.declared_ciphertext_bytes) {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "attachment_too_large",
        ));
    }
    // Cheap DB-only fencing so this vault's expired reservations stop counting
    // immediately; object deletion is left to the retrying worker.
    retire_expired_reservations(&s.db, Some(principal.vault), 64)
        .await
        .map_err(|error| database_unavailable(&error, "attachment_reservation_retire"))?;
    let attachment_id = request.attachment_id.unwrap_or_else(Uuid::new_v4);
    // Placeholder key: never written. Each upload attempt gets its own key.
    let key = format!("private/{}/{}", principal.vault, Uuid::new_v4());
    let mut tx =
        s.db.begin()
            .await
            .map_err(|error| database_unavailable(&error, "attachment_reserve_begin"))?;
    // Serializing on the vault row makes aggregate quota admission race-free.
    lock_vault(&mut tx, principal.vault).await?;
    if let Some(existing) = sqlx::query("SELECT device_id,declared_bytes,declared_sha256,expires_at > now() AS live,deleting_at IS NULL AS available FROM upload_reservations WHERE vault_id=$1 AND attachment_id=$2")
        .bind(principal.vault).bind(attachment_id).fetch_optional(&mut *tx).await.map_err(|error| database_unavailable(&error, "attachment_reserve_duplicate"))? {
        let same = existing.get::<Uuid, _>("device_id") == principal.device
            && existing.get::<i64, _>("declared_bytes") == request.declared_ciphertext_bytes
            && existing.get::<Vec<u8>, _>("declared_sha256") == hash
            && existing.get::<bool, _>("live") && existing.get::<bool, _>("available");
        tx.commit().await.map_err(|error| database_unavailable(&error, "attachment_reserve_commit"))?;
        if same { return Ok(Json(ReservedAttachment { attachment_id, expires_in_seconds: RESERVATION_TTL_SECONDS })); }
        return Err(ApiError(StatusCode::CONFLICT, "attachment_reservation_conflict"));
    }
    let used = quota_used(&mut tx, principal.vault).await?;
    if used
        .checked_add(request.declared_ciphertext_bytes)
        .is_none_or(|total| total > s.vault_attachment_quota_bytes)
    {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "vault_quota_exceeded",
        ));
    }
    sqlx::query("INSERT INTO upload_reservations(vault_id,attachment_id,device_id,object_key,declared_bytes,declared_sha256) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(principal.vault).bind(attachment_id).bind(principal.device).bind(key).bind(request.declared_ciphertext_bytes).bind(hash)
        .execute(&mut *tx).await.map_err(|error| insert_error(error, "attachment_reserve"))?;
    tx.commit()
        .await
        .map_err(|error| database_unavailable(&error, "attachment_reserve_commit"))?;
    Ok(Json(ReservedAttachment {
        attachment_id,
        expires_in_seconds: RESERVATION_TTL_SECONDS,
    }))
}

const OPEN_RESERVATION: &str = "vault_id=$1 AND attachment_id=$2 AND device_id=$3 AND finalized_at IS NULL AND deleting_at IS NULL AND expires_at > now()";

pub(super) async fn upload_attachment(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(attachment_id): Path<Uuid>,
    body: Body,
) -> ApiResult<StatusCode> {
    let principal = auth(&s.db, &h).await?;
    let store = storage(&s)?;
    let _slot = upload_slot(&s)?;
    let row = sqlx::query(&format!(
        "SELECT declared_bytes,declared_sha256 FROM upload_reservations WHERE {OPEN_RESERVATION}"
    ))
    .bind(principal.vault)
    .bind(attachment_id)
    .bind(principal.device)
    .fetch_optional(&s.db)
    .await
    .map_err(|error| database_unavailable(&error, "attachment_upload_lookup"))?
    .ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "upload_reservation_not_found",
    ))?;
    let declared: i64 = row.get("declared_bytes");
    let expected: Vec<u8> = row.get("declared_sha256");
    let mut spool = Spool::create().await?;
    let (received, actual, _) = spool
        .fill(
            body,
            declared.min(MAX_ATTACHMENT_BYTES),
            "attachment_too_large",
            0,
        )
        .await?;
    if received != declared || actual != expected {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "ciphertext_mismatch",
        ));
    }
    // A fresh key per attempt: a slower duplicate PUT can never overwrite or
    // delete the object another attempt attached or finalized.
    let key = format!("private/{}/{}", principal.vault, Uuid::new_v4());
    record_intent(&s.db, &key, principal.vault, "upload_attempt").await?;
    if let Err(error) = store.put_file(&key, spool.path.clone()).await {
        queue_now(&s.db, &key, principal.vault, "upload_attempt").await;
        return Err(storage_failure(
            error,
            ApiError(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"),
            "attachment_upload_put",
        ));
    }
    drop(spool);
    match attach_upload(
        &s.db,
        principal.vault,
        principal.device,
        attachment_id,
        &key,
        received,
        &actual,
    )
    .await
    {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(error) => {
            queue_now(&s.db, &key, principal.vault, "upload_attempt").await;
            Err(error)
        }
    }
}

/// Makes `key` the reservation's object if the reservation is still open and
/// the attempt intent is still unclaimed. The previously attached attempt (if
/// any) is queued for deletion in the same transaction.
async fn attach_upload(
    db: &PgPool,
    vault: Uuid,
    device: Uuid,
    attachment_id: Uuid,
    key: &str,
    bytes: i64,
    sha256: &[u8],
) -> ApiResult<()> {
    let mut tx = db
        .begin()
        .await
        .map_err(|error| database_unavailable(&error, "attachment_upload_begin"))?;
    let previous = sqlx::query(&format!("SELECT object_key,uploaded_at IS NOT NULL AS uploaded FROM upload_reservations WHERE {OPEN_RESERVATION} FOR UPDATE"))
        .bind(vault)
        .bind(attachment_id)
        .bind(device)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "attachment_upload_lock"))?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "upload_reservation_not_found"))?;
    if !claim_intent(&mut tx, key).await? {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
        ));
    }
    if previous.get::<bool, _>("uploaded") {
        sqlx::query(QUEUE_NOW)
            .bind(previous.get::<String, _>("object_key"))
            .bind(vault)
            .bind("superseded_upload")
            .execute(&mut *tx)
            .await
            .map_err(|error| database_unavailable(&error, "attachment_upload_supersede"))?;
    }
    sqlx::query("UPDATE upload_reservations SET object_key=$4,uploaded_bytes=$5,uploaded_sha256=$6,uploaded_at=now() WHERE vault_id=$1 AND attachment_id=$2 AND device_id=$3")
        .bind(vault)
        .bind(attachment_id)
        .bind(device)
        .bind(key)
        .bind(bytes)
        .bind(sha256)
        .execute(&mut *tx)
        .await
        .map_err(|error| database_unavailable(&error, "attachment_upload_mark"))?;
    tx.commit()
        .await
        .map_err(|error| database_unavailable(&error, "attachment_upload_commit"))
}

pub(super) async fn finalize_attachment(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(attachment_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let principal = auth(&s.db, &h).await?;
    let store = storage(&s)?;
    let _slot = upload_slot(&s)?;
    let unavailable = ApiError(StatusCode::CONFLICT, "attachment_unavailable");
    // A concurrent re-upload can replace the object between the unlocked HEAD
    // and the locked commit; re-verify the new object a bounded number of times.
    for _ in 0..3 {
        let row = sqlx::query("SELECT object_key,declared_bytes,finalized_at IS NOT NULL AS finalized,uploaded_at IS NOT NULL AS uploaded FROM upload_reservations WHERE vault_id=$1 AND attachment_id=$2 AND device_id=$3 AND deleting_at IS NULL AND (finalized_at IS NOT NULL OR expires_at > now())")
            .bind(principal.vault).bind(attachment_id).bind(principal.device)
            .fetch_optional(&s.db).await
            .map_err(|error| database_unavailable(&error, "attachment_finalize_lookup"))?
            .ok_or(ApiError(StatusCode::NOT_FOUND, "upload_reservation_not_found"))?;
        if row.get::<bool, _>("finalized") {
            return Ok(Json(
                json!({"attachment_id":attachment_id,"duplicate":true}),
            ));
        }
        if !row.get::<bool, _>("uploaded") {
            return Err(unavailable);
        }
        let key: String = row.get("object_key");
        let bytes: i64 = row.get("declared_bytes");
        // Bounded storage HEAD with no database lock held.
        let stored = store.head_bytes(&key).await.map_err(|error| {
            storage_failure(
                error,
                ApiError(StatusCode::CONFLICT, "attachment_unavailable"),
                "attachment_finalize_head",
            )
        })?;
        if stored != bytes {
            return Err(unavailable);
        }
        let mut tx =
            s.db.begin()
                .await
                .map_err(|error| database_unavailable(&error, "attachment_finalize_begin"))?;
        let locked = sqlx::query("SELECT object_key,declared_sha256,finalized_at IS NOT NULL AS finalized FROM upload_reservations WHERE vault_id=$1 AND attachment_id=$2 AND device_id=$3 AND deleting_at IS NULL AND (finalized_at IS NOT NULL OR expires_at > now()) FOR UPDATE")
            .bind(principal.vault).bind(attachment_id).bind(principal.device)
            .fetch_optional(&mut *tx).await
            .map_err(|error| database_unavailable(&error, "attachment_finalize_lock"))?
            .ok_or(ApiError(StatusCode::NOT_FOUND, "upload_reservation_not_found"))?;
        if locked.get::<bool, _>("finalized") {
            return Ok(Json(
                json!({"attachment_id":attachment_id,"duplicate":true}),
            ));
        }
        if locked.get::<String, _>("object_key") != key {
            continue;
        }
        sqlx::query("INSERT INTO attachments(vault_id,attachment_id,object_key,ciphertext_bytes,ciphertext_sha256,created_by_device_id) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(principal.vault).bind(attachment_id).bind(&key).bind(bytes)
            .bind(locked.get::<Vec<u8>, _>("declared_sha256")).bind(principal.device)
            .execute(&mut *tx).await
            .map_err(|error| insert_error(error, "attachment_finalize_insert"))?;
        sqlx::query("UPDATE upload_reservations SET finalized_at=now() WHERE vault_id=$1 AND attachment_id=$2")
            .bind(principal.vault).bind(attachment_id)
            .execute(&mut *tx).await
            .map_err(|error| database_unavailable(&error, "attachment_finalize_mark"))?;
        tx.commit()
            .await
            .map_err(|error| database_unavailable(&error, "attachment_finalize_commit"))?;
        return Ok(Json(
            json!({"attachment_id":attachment_id,"duplicate":false}),
        ));
    }
    Err(ApiError(
        StatusCode::CONFLICT,
        "attachment_upload_in_progress",
    ))
}

pub(super) async fn download_attachment(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(attachment_id): Path<Uuid>,
) -> ApiResult<Response> {
    let principal = auth(&s.db, &h).await?;
    let store = storage(&s)?;
    let key: String = sqlx::query_scalar(
        "SELECT object_key FROM attachments WHERE vault_id=$1 AND attachment_id=$2",
    )
    .bind(principal.vault)
    .bind(attachment_id)
    .fetch_optional(&s.db)
    .await
    .map_err(|error| database_unavailable(&error, "attachment_download_lookup"))?
    .ok_or(ApiError(StatusCode::NOT_FOUND, "attachment_not_found"))?;
    let stream = store.get(&key).await.map_err(|error| {
        storage_failure(
            error,
            ApiError(StatusCode::CONFLICT, "attachment_unavailable"),
            "attachment_download",
        )
    })?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("content-disposition", "attachment")
        .header("x-content-type-options", "nosniff")
        .body(Body::from_stream(stream))
        .expect("static response headers"))
}

fn public_token() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

fn safe_public_name(name: &str) -> Option<String> {
    let name = name.rsplit(['/', '\\']).next()?.trim();
    if name.is_empty() || name.len() > 100 || name == "." || name == ".." {
        return None;
    }
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    (!cleaned.is_empty() && cleaned != "." && cleaned != "..").then_some(cleaned)
}

fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub(super) async fn create_public_copy(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(attachment_id): Path<Uuid>,
    body: Body,
) -> ApiResult<Json<Value>> {
    let principal = auth(&s.db, &h).await?;
    let store = storage(&s)?;
    let _slot = upload_slot(&s)?;
    // Any active device of the vault may derive a public copy of a finalized attachment it can
    // already read (for example a received MMS image). The copy is a separate, client re-encoded
    // object owned by the requester; the private original is never promoted or exposed.
    // Revocation remains creator/owner-only, and other vaults' attachments stay invisible.
    let readable: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM attachments WHERE vault_id=$1 AND attachment_id=$2")
            .bind(principal.vault)
            .bind(attachment_id)
            .fetch_optional(&s.db)
            .await
            .map_err(|error| database_unavailable(&error, "public_copy_attachment_lookup"))?;
    if readable.is_none() {
        return Err(ApiError(StatusCode::NOT_FOUND, "attachment_not_found"));
    }
    let name = h
        .get("x-file-name")
        .and_then(|value| value.to_str().ok())
        .and_then(safe_public_name)
        .ok_or(ApiError(StatusCode::BAD_REQUEST, "invalid_public_filename"))?;
    let mut spool = Spool::create().await?;
    let (bytes, _, prefix) = spool
        .fill(body, MAX_PUBLIC_COPY_BYTES, "public_copy_too_large", 12)
        .await?;
    let media_type = image_media_type(&prefix).ok_or(ApiError(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_public_media",
    ))?;
    let token = public_token();
    let digest = Sha256::digest(token.as_bytes());
    let share_id = Uuid::new_v4();
    let key = format!("public/{}/{}", principal.vault, Uuid::new_v4());

    // Admission: quota check, pending row and attempt intent commit together
    // under the vault lock, so concurrent copies cannot overbook.
    let mut tx =
        s.db.begin()
            .await
            .map_err(|error| database_unavailable(&error, "public_copy_begin"))?;
    lock_vault(&mut tx, principal.vault).await?;
    let used = quota_used(&mut tx, principal.vault).await?;
    if used
        .checked_add(bytes)
        .is_none_or(|total| total > s.vault_attachment_quota_bytes)
    {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "vault_quota_exceeded",
        ));
    }
    sqlx::query("INSERT INTO public_attachment_copies(share_id,vault_id,attachment_id,object_key,token_digest,safe_name,media_type,byte_count,created_by_device_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(share_id).bind(principal.vault).bind(attachment_id).bind(&key).bind(digest.as_slice()).bind(&name).bind(media_type).bind(bytes).bind(principal.device)
        .execute(&mut *tx).await.map_err(|error| database_unavailable(&error, "public_copy_insert"))?;
    sqlx::query("INSERT INTO storage_deletions(object_key,vault_id,reason,not_before) VALUES($1,$2,'public_copy',now() + ($3::bigint * interval '1 second'))")
        .bind(&key).bind(principal.vault).bind(ATTEMPT_INTENT_GRACE_SECONDS)
        .execute(&mut *tx).await.map_err(|error| database_unavailable(&error, "public_copy_intent"))?;
    tx.commit()
        .await
        .map_err(|error| database_unavailable(&error, "public_copy_admit_commit"))?;

    if let Err(error) = store.put_file(&key, spool.path.clone()).await {
        retire_failed_copy(&s.db, share_id, &key, principal.vault).await;
        return Err(storage_failure(
            error,
            ApiError(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"),
            "public_copy_put",
        ));
    }
    drop(spool);
    let ready = async {
        let mut tx =
            s.db.begin()
                .await
                .map_err(|error| database_unavailable(&error, "public_copy_ready_begin"))?;
        if !claim_intent(&mut tx, &key).await? {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
            ));
        }
        let updated = sqlx::query("UPDATE public_attachment_copies SET ready_at=now() WHERE share_id=$1 AND retired_at IS NULL")
            .bind(share_id).execute(&mut *tx).await
            .map_err(|error| database_unavailable(&error, "public_copy_ready"))?;
        if updated.rows_affected() != 1 {
            return Err(ApiError(StatusCode::NOT_FOUND, "public_copy_not_found"));
        }
        tx.commit()
            .await
            .map_err(|error| database_unavailable(&error, "public_copy_ready_commit"))
    };
    if let Err(error) = ready.await {
        retire_failed_copy(&s.db, share_id, &key, principal.vault).await;
        return Err(error);
    }
    Ok(Json(
        json!({"share_id":share_id,"token":token,"safe_name":name,"expires_in_seconds":604800}),
    ))
}

async fn retire_failed_copy(db: &PgPool, share_id: Uuid, key: &str, vault: Uuid) {
    if let Err(error) =
        sqlx::query("UPDATE public_attachment_copies SET retired_at=COALESCE(retired_at,now()) WHERE share_id=$1")
            .bind(share_id)
            .execute(db)
            .await
    {
        tracing::warn!(error_kind = %error, "public copy retire failed; intent grace still applies");
    }
    queue_now(db, key, vault, "public_copy").await;
}

pub(super) async fn revoke_public_copy(
    State(s): State<ApiState>,
    h: HeaderMap,
    Path(share_id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let principal = auth(&s.db, &h).await?;
    // Revocation and deletion enqueue are one transaction (fenced deletion).
    let revoked = sqlx::query("WITH revoked AS (UPDATE public_attachment_copies SET revoked_at=now(),retired_at=COALESCE(retired_at,now()) WHERE share_id=$1 AND vault_id=$2 AND (created_by_device_id=$3 OR $4='owner') AND revoked_at IS NULL RETURNING object_key,vault_id) INSERT INTO storage_deletions(object_key,vault_id,reason) SELECT object_key,vault_id,'public_copy' FROM revoked ON CONFLICT (object_key) DO UPDATE SET not_before=now() RETURNING 1")
        .bind(share_id).bind(principal.vault).bind(principal.device).bind(&principal.role)
        .fetch_optional(&s.db).await.map_err(|error| database_unavailable(&error, "public_copy_revoke"))?;
    if revoked.is_none() {
        return Err(ApiError(StatusCode::NOT_FOUND, "public_copy_not_found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn download_public_copy(
    State(s): State<ApiState>,
    Path((token, safe_name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let not_found = || ApiError(StatusCode::NOT_FOUND, "public_copy_not_found");
    if token.len() != 43
        || URL_SAFE_NO_PAD
            .decode(&token)
            .ok()
            .is_none_or(|bytes| bytes.len() != 32)
    {
        return Err(not_found());
    }
    let digest = Sha256::digest(token.as_bytes());
    let row = sqlx::query("SELECT object_key,safe_name,media_type FROM public_attachment_copies WHERE token_digest=$1 AND revoked_at IS NULL AND retired_at IS NULL AND ready_at IS NOT NULL AND expires_at > now()")
        .bind(digest.as_slice()).fetch_optional(&s.db).await.map_err(|error| database_unavailable(&error, "public_copy_lookup"))?.ok_or_else(not_found)?;
    let stored_name: String = row.get("safe_name");
    if safe_name != stored_name {
        return Err(not_found());
    }
    let key: String = row.get("object_key");
    let media_type: String = row.get("media_type");
    let stream = storage(&s)?
        .get(&key)
        .await
        .map_err(|error| storage_failure(error, not_found(), "public_copy_download"))?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", media_type)
        .header(
            "content-disposition",
            format!("attachment; filename=\"{stored_name}\""),
        )
        .header("x-content-type-options", "nosniff")
        .header("content-security-policy", "sandbox; default-src 'none'")
        .header("referrer-policy", "no-referrer")
        .header("cache-control", "private, no-store")
        .body(Body::from_stream(stream))
        .expect("validated response headers"))
}

/// Removes expired, unfinalized reservations (optionally one vault) and queues
/// any uploaded object in the same statement. Bounded by `limit`.
pub(super) async fn retire_expired_reservations(
    db: &PgPool,
    vault: Option<Uuid>,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("WITH expired AS (DELETE FROM upload_reservations WHERE ctid IN (SELECT ctid FROM upload_reservations WHERE finalized_at IS NULL AND deleting_at IS NULL AND expires_at <= now() AND ($1::uuid IS NULL OR vault_id=$1) ORDER BY expires_at LIMIT $2 FOR UPDATE SKIP LOCKED) RETURNING vault_id,object_key,uploaded_at) INSERT INTO storage_deletions(object_key,vault_id,reason) SELECT object_key,vault_id,'expired_reservation' FROM expired WHERE uploaded_at IS NOT NULL ON CONFLICT (object_key) DO UPDATE SET not_before=now()")
        .bind(vault)
        .bind(limit)
        .execute(db)
        .await?
        .rows_affected())
}

/// Retires expired public copies and queues their objects in one statement.
pub(super) async fn retire_expired_public_copies(
    db: &PgPool,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("WITH retired AS (UPDATE public_attachment_copies SET retired_at=now() WHERE ctid IN (SELECT ctid FROM public_attachment_copies WHERE retired_at IS NULL AND expires_at <= now() ORDER BY expires_at LIMIT $1 FOR UPDATE SKIP LOCKED) RETURNING object_key,vault_id) INSERT INTO storage_deletions(object_key,vault_id,reason) SELECT object_key,vault_id,'public_copy' FROM retired ON CONFLICT (object_key) DO UPDATE SET not_before=now()")
        .bind(limit)
        .execute(db)
        .await?
        .rows_affected())
}

/// Exponential retry delay for failed object deletes.
pub(super) fn delete_backoff_seconds(attempts: i32) -> i64 {
    1_i64
        .checked_shl(attempts.clamp(0, 30) as u32)
        .unwrap_or(MAX_DELETE_BACKOFF_SECONDS)
        .min(MAX_DELETE_BACKOFF_SECONDS)
}

/// Outcome of one deletion pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeletionStats {
    pub deleted: u64,
    pub retried: u64,
    pub skipped_referenced: u64,
}

/// Deletes due queued objects that nothing references. Claimed rows stay
/// locked during their bounded storage delete, so an attempt cannot be
/// attached and reaped concurrently. Failures are retried with backoff.
pub(super) async fn process_storage_deletions(
    db: &PgPool,
    store: &Storage,
    limit: i64,
) -> Result<DeletionStats, sqlx::Error> {
    let mut stats = DeletionStats::default();
    let mut tx = db.begin().await?;
    let due = sqlx::query("SELECT d.object_key,d.attempts,(EXISTS (SELECT 1 FROM attachments a WHERE a.object_key=d.object_key) OR EXISTS (SELECT 1 FROM upload_reservations r WHERE r.object_key=d.object_key AND r.uploaded_at IS NOT NULL) OR EXISTS (SELECT 1 FROM public_attachment_copies p WHERE p.object_key=d.object_key AND p.retired_at IS NULL AND p.ready_at IS NOT NULL)) AS referenced FROM storage_deletions d WHERE d.not_before <= now() ORDER BY d.not_before LIMIT $1 FOR UPDATE OF d SKIP LOCKED")
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
    for row in due {
        let key: String = row.get("object_key");
        if row.get::<bool, _>("referenced") {
            // Never delete a live object; the queue entry was stale.
            tracing::warn!("skipped deletion of a referenced storage object");
            sqlx::query("DELETE FROM storage_deletions WHERE object_key=$1")
                .bind(&key)
                .execute(&mut *tx)
                .await?;
            stats.skipped_referenced += 1;
            continue;
        }
        match store.delete(&key).await {
            Ok(()) => {
                sqlx::query("DELETE FROM storage_deletions WHERE object_key=$1")
                    .bind(&key)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE public_attachment_copies SET retired_at=COALESCE(retired_at,now()),purged_at=now() WHERE object_key=$1")
                    .bind(&key)
                    .execute(&mut *tx)
                    .await?;
                stats.deleted += 1;
            }
            Err(error) => {
                tracing::warn!(error_kind = %error, "storage delete failed; will retry");
                sqlx::query("UPDATE storage_deletions SET attempts=attempts+1,not_before=now() + ($2::bigint * interval '1 second') WHERE object_key=$1")
                    .bind(&key)
                    .bind(delete_backoff_seconds(row.get::<i32, _>("attempts")))
                    .execute(&mut *tx)
                    .await?;
                stats.retried += 1;
            }
        }
    }
    tx.commit().await?;
    Ok(stats)
}

/// One bounded storage maintenance pass: retire expired reservations and
/// copies, then process due deletions.
pub(super) async fn storage_maintenance(
    db: &PgPool,
    store: &Storage,
    limit: i64,
) -> Result<DeletionStats, sqlx::Error> {
    retire_expired_reservations(db, None, limit).await?;
    retire_expired_public_copies(db, limit).await?;
    process_storage_deletions(db, store, limit).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_backoff_grows_and_is_capped() {
        assert_eq!(delete_backoff_seconds(0), 1);
        assert_eq!(delete_backoff_seconds(3), 8);
        assert_eq!(delete_backoff_seconds(12), MAX_DELETE_BACKOFF_SECONDS);
        assert_eq!(delete_backoff_seconds(i32::MAX), MAX_DELETE_BACKOFF_SECONDS);
        assert_eq!(delete_backoff_seconds(-5), 1);
    }

    #[test]
    fn public_names_and_media_are_restricted() {
        assert_eq!(
            safe_public_name("../photo.png").as_deref(),
            Some("photo.png")
        );
        assert_eq!(safe_public_name(".."), None);
        assert_eq!(image_media_type(b"<svg/>"), None);
        assert_eq!(image_media_type(b"\x89PNG\r\n\x1a\nx"), Some("image/png"));
    }

    #[tokio::test]
    async fn spool_file_is_removed_when_dropped_mid_request() {
        let spool = Spool::create().await.unwrap();
        let path = spool.path.clone();
        assert!(path.exists());
        drop(spool);
        assert!(!path.exists());
    }
}
