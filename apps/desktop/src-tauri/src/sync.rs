//! Native network supervisor: bounded HTTP catch-up (outbox, media, replay, staged snapshot
//! resync) plus a persistent authenticated WebSocket (`ready` / `event` / `resync_required`)
//! with bounded, jittered reconnect and cooperative cancellation. Every client-core call runs
//! in `spawn_blocking`; the Tokio workers never block on SQLCipher, Argon2 or file crypto.
use crate::{
    error::{core_error, BridgeError, BridgeResult},
    gateways::{gateway_views, CapabilitiesResponse, DevicesResponse},
    net::{NetError, MAX_JSON_BYTES, MAX_PAGE_BYTES},
    origin::websocket_url,
    session::{Session, VaultSummary},
};
use futures_util::{SinkExt, StreamExt};
use openpush_client_core::{
    Client, Cursor, EnvelopePurpose, KeyProfile, RawSnapshotRecord, SnapshotPurpose,
    VaultCheckHeader, MAX_APPLY_BATCH, MAX_SEAL_BATCH, MAX_SNAPSHOT_PAGE,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::{sync::Arc, time::Duration};
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::tungstenite::{
    self, client::IntoClientRequest, protocol::WebSocketConfig, Message,
};

const MAX_BACKOFF: Duration = Duration::from_secs(60);
const WORK_INTERVAL: Duration = Duration::from_secs(15);
const GATEWAY_REFRESH_EVERY: u32 = 4;
const WS_MAX_MESSAGE: usize = 2 * 1024 * 1024;
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WS_READY_TIMEOUT: Duration = Duration::from_secs(10);
const WS_PING_AFTER: Duration = Duration::from_secs(45);
const WS_IDLE_LIMIT: Duration = Duration::from_secs(120);
const MAX_REPLAY_PAGES_PER_ROUND: usize = 20;
const MAX_OUTBOX_BATCHES: usize = 8;
const OUTBOX_BATCH: usize = 32;
const MEDIA_PER_ROUND: usize = 4;
/// Apply steps per normal pass; snapshot drains continue until `snapshot_remaining == 0`.
const APPLY_STEPS: usize = 8;
const WS_CLOSE_REVOKED: u16 = 4401;
const WS_CLOSE_RESYNC: u16 = 4409;

/// Runs client-core work off the async workers.
pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> BridgeResult<T> + Send + 'static,
) -> BridgeResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| BridgeError::host_state())?
}

fn client(session: &Arc<Session>) -> Client {
    session.client.clone()
}

#[derive(Deserialize)]
pub struct VaultResponse {
    pub vault_id: String,
    pub device_id: String,
    pub public_key_profile: serde_json::Value,
    pub encrypted_vault_check_header: String,
    pub profile_fingerprint: String,
}

/// Authenticated `/v1/vault`, verifying that the server's principal is exactly this binding.
pub async fn fetch_vault(
    api: &crate::net::Api,
    vault_id: &str,
    device_id: &str,
) -> BridgeResult<VaultResponse> {
    let vault: VaultResponse = api.get_json("/v1/vault", MAX_JSON_BYTES).await?;
    let same = |a: &str, b: &str| {
        uuid::Uuid::parse_str(a)
            .ok()
            .zip(uuid::Uuid::parse_str(b).ok())
            .is_some_and(|(a, b)| a == b)
    };
    if !same(&vault.vault_id, vault_id) || !same(&vault.device_id, device_id) {
        return Err(BridgeError::new(
            "credential-mismatch",
            "The credential does not match the authenticated server device.",
        ));
    }
    Ok(vault)
}

pub fn vault_header(vault: &VaultResponse) -> BridgeResult<(KeyProfile, VaultCheckHeader)> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let invalid = |m: &'static str| BridgeError::new("server-response", m);
    let profile: KeyProfile = serde_json::from_value(vault.public_key_profile.clone())
        .map_err(|_| invalid("The server key profile is invalid."))?;
    let bytes = STANDARD
        .decode(vault.encrypted_vault_check_header.as_bytes())
        .map_err(|_| invalid("The server vault header is invalid."))?;
    let header: VaultCheckHeader = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("The server vault header has an unsupported encoding."))?;
    Ok((profile, header))
}

async fn refresh_vault(session: &Arc<Session>) -> BridgeResult<()> {
    let vault = fetch_vault(
        &session.api,
        &session.binding.vault_id,
        &session.binding.device_id,
    )
    .await?;
    let (profile, _) = vault_header(&vault)?;
    session.set_status(|status| {
        status.vault = Some(VaultSummary {
            epoch: profile.key_epoch,
            fingerprint: vault.profile_fingerprint.clone(),
        })
    });
    Ok(())
}

pub async fn refresh_gateways(session: &Arc<Session>) -> BridgeResult<()> {
    let devices: DevicesResponse = session.api.get_json("/v1/devices", MAX_JSON_BYTES).await?;
    let capabilities: CapabilitiesResponse = session
        .api
        .get_json("/v1/capabilities", MAX_JSON_BYTES)
        .await?;
    let views = gateway_views(&devices, &capabilities);
    let changed = {
        let mut status = session.status.lock().unwrap_or_else(|p| p.into_inner());
        let changed = !status.gateways_known || status.gateways != views;
        status.gateways = views.clone();
        status.gateways_known = true;
        changed
    };
    if changed {
        session.persist_gateways(&views);
        session.notify();
    }
    Ok(())
}

/// Drains published snapshot records and applies journaled records in bounded steps, yielding
/// between steps. `applied == 0` alone never means complete: the loop continues while history is
/// still being drained (`snapshot_remaining > 0`) unless `max_steps` is reached or cancelled.
pub async fn drain_apply(session: &Arc<Session>, max_steps: usize) -> BridgeResult<bool> {
    let mut progressed = false;
    for _ in 0..max_steps {
        if session.cancel.is_cancelled() {
            break;
        }
        let c = client(session);
        let report = blocking(move || c.apply_pending(MAX_APPLY_BATCH).map_err(core_error)).await?;
        let step = report.applied + report.drained + report.quarantined > 0;
        progressed |= step;
        // No progress at all ends the pass (history waiting for keys stays journaled); while
        // records are still being drained or applied the loop continues.
        if !step {
            break;
        }
        tokio::task::yield_now().await;
    }
    if progressed {
        session.notify();
    }
    Ok(progressed)
}

#[derive(Deserialize)]
struct RawRecord {
    cursor: String,
    envelope: Box<RawValue>,
}
#[derive(Deserialize)]
struct EventsPage {
    events: Vec<RawRecord>,
}
#[derive(Deserialize)]
struct SnapshotStart {
    high_water_cursor: String,
    record_count: String,
    #[serde(default)]
    max_page_size: Option<u64>,
}
#[derive(Deserialize)]
struct SnapshotPage {
    records: Vec<RawRecord>,
}

fn parse_cursor(value: &str) -> BridgeResult<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|c| *c >= 1 && *c <= i64::MAX as u64)
        .ok_or_else(|| NetError::Invalid.into())
}

/// Journals raw envelope bytes; malformed records reach core quarantine instead of failing a
/// typed parse here.
async fn ingest_page(session: &Arc<Session>, records: Vec<RawRecord>) -> BridgeResult<()> {
    let parsed = records
        .into_iter()
        .map(|r| {
            Ok((
                parse_cursor(&r.cursor)?,
                r.envelope.get().as_bytes().to_vec(),
            ))
        })
        .collect::<BridgeResult<Vec<_>>>()?;
    let c = client(session);
    blocking(move || {
        for (cursor, bytes) in parsed {
            c.ingest_raw(&bytes, Cursor(cursor)).map_err(core_error)?;
        }
        Ok(())
    })
    .await
}

/// Staged snapshot import after `resync_required`; resumes a persisted generation when possible.
/// Records are passed raw so invalid ones are quarantined by core. Snapshot history is merge-only
/// and its commands are historical (never executable); the restore guard is never touched.
pub(crate) async fn snapshot_resync(session: &Arc<Session>) -> BridgeResult<()> {
    // A previously published generation must finish draining before a new one may begin.
    drain_apply(session, usize::MAX).await?;
    let mut fresh = false;
    for _ in 0..MAX_SNAPSHOT_ATTEMPTS {
        let progress = match start_or_resume(session, fresh).await {
            Ok(progress) => progress,
            Err(error) => return Err(error),
        };
        match import_cut(session, progress).await {
            Ok(()) => return Ok(()),
            Err(CutError::Impossible) => {
                // The staged cut can never complete (server moved/rolled back, or ended early):
                // never resume it again; `begin_snapshot` obsoletes it on the next attempt.
                *session
                    .abandoned_snapshot
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some(progress.generation);
                fresh = true;
            }
            Err(CutError::Other(error)) => return Err(error),
        }
    }
    Err(BridgeError::new("resync-required", "The server snapshot kept changing; history import will be retried. Live state is unchanged."))
}

const MAX_SNAPSHOT_ATTEMPTS: usize = 3;

enum CutError {
    Impossible,
    Other(BridgeError),
}

impl From<BridgeError> for CutError {
    fn from(error: BridgeError) -> Self {
        // Core reports an inconsistent/stale generation as `SnapshotMismatch`.
        if error.code == "resync-required" {
            CutError::Impossible
        } else {
            CutError::Other(error)
        }
    }
}

async fn start_or_resume(
    session: &Arc<Session>,
    fresh: bool,
) -> BridgeResult<openpush_client_core::SnapshotProgress> {
    if !fresh {
        let c = client(session);
        let abandoned = *session
            .abandoned_snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let resume = blocking(move || c.snapshot_progress().map_err(core_error))
            .await?
            .filter(|p| p.received_records < p.expected_records && Some(p.generation) != abandoned);
        if let Some(progress) = resume {
            return Ok(progress);
        }
    }
    let start: SnapshotStart = session.api.get_json("/v1/snapshot", MAX_JSON_BYTES).await?;
    let invalid = || BridgeError::from(NetError::Invalid);
    let high_water = start
        .high_water_cursor
        .parse::<u64>()
        .map_err(|_| invalid())?;
    let count = start.record_count.parse::<u64>().map_err(|_| invalid())?;
    let _ = start.max_page_size;
    let c = client(session);
    blocking(move || {
        c.begin_snapshot(Cursor(high_water), count, SnapshotPurpose::Resync)
            .map_err(core_error)
    })
    .await
}

/// Pages one fixed cut into core staging and publishes it. Returns `Impossible` when this cut
/// cannot be completed (typed resync/invalid cursor from the server, an early end, or a core
/// mismatch), so the caller starts a new cut instead of retrying the same one forever.
async fn import_cut(
    session: &Arc<Session>,
    mut progress: openpush_client_core::SnapshotProgress,
) -> Result<(), CutError> {
    let mut limit = MAX_SNAPSHOT_PAGE.min(200);
    while progress.received_records < progress.expected_records {
        if session.cancel.is_cancelled() {
            return Ok(());
        }
        let path = format!(
            "/v1/snapshot/records?high_water={}&after={}&limit={limit}",
            progress.high_water.0, progress.last_cursor.0
        );
        let page: SnapshotPage = match session.api.get_json(&path, MAX_PAGE_BYTES).await {
            Ok(page) => page,
            Err(NetError::TooLarge) if limit > 1 => {
                limit /= 2;
                continue;
            }
            Err(NetError::Status {
                status: 409 | 400, ..
            }) => return Err(CutError::Impossible),
            Err(error) => return Err(CutError::Other(error.into())),
        };
        if page.records.is_empty() {
            return Err(CutError::Impossible);
        }
        let records = page
            .records
            .into_iter()
            .map(|r| {
                Ok(RawSnapshotRecord {
                    cursor: Cursor(parse_cursor(&r.cursor)?),
                    envelope_json: r.envelope.get().as_bytes().to_vec(),
                })
            })
            .collect::<BridgeResult<Vec<_>>>()?;
        let (c, generation) = (client(session), progress.generation);
        match blocking(move || {
            c.append_snapshot_raw_page(generation, &records)
                .map_err(core_error)
        })
        .await
        {
            Ok(next) => progress = next,
            // Page byte cap: retry the same position with a smaller page.
            Err(error) if error.code == "invalid-request" && limit > 1 => limit /= 2,
            Err(error) => return Err(error.into()),
        }
        tokio::task::yield_now().await;
    }
    let (c, generation) = (client(session), progress.generation);
    blocking(move || c.finish_snapshot(generation).map_err(core_error)).await?;
    drain_apply(session, usize::MAX).await?;
    Ok(())
}

/// HTTP replay after the durable receive cursor (bounded pages, adaptive size).
async fn replay(session: &Arc<Session>) -> BridgeResult<()> {
    let mut limit = 100usize;
    let mut pages = 0;
    while pages < MAX_REPLAY_PAGES_PER_ROUND && !session.cancel.is_cancelled() {
        let c = client(session);
        let after = blocking(move || c.receive_cursor().map_err(core_error))
            .await?
            .0;
        let page: EventsPage = match session
            .api
            .get_json(
                &format!("/v1/events?after={after}&limit={limit}"),
                MAX_PAGE_BYTES,
            )
            .await
        {
            Ok(page) => page,
            Err(error) if error.is_status(409, "resync_required") => {
                snapshot_resync(session).await?;
                pages += 1;
                continue;
            }
            Err(NetError::TooLarge) if limit > 1 => {
                limit /= 2;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let count = page.events.len();
        if count == 0 {
            break;
        }
        ingest_page(session, page.events).await?;
        drain_apply(session, APPLY_STEPS).await?;
        pages += 1;
        if count < limit {
            break;
        }
    }
    Ok(())
}

fn record_work_error(session: &Arc<Session>, code: &'static str) {
    session.set_status(|status| status.work_error = Some(code));
}

fn transport_fatal(error: &NetError) -> bool {
    matches!(error, NetError::Offline | NetError::Revoked)
}

/// How a media transfer failure is handled.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MediaOutcome {
    /// Stop this round (offline or revoked).
    Fatal,
    /// Transient server condition (timeouts, 429, 5xx): retried next round, never cached.
    Retry,
    /// Deterministic rejection or verification failure: skipped until restart and shown.
    Permanent,
}

pub(crate) enum Failure {
    Net(NetError),
    Permanent(BridgeError),
}
impl From<NetError> for Failure {
    fn from(error: NetError) -> Self {
        Failure::Net(error)
    }
}

pub(crate) fn classify(failure: &Failure) -> MediaOutcome {
    match failure {
        Failure::Net(error) if transport_fatal(error) => MediaOutcome::Fatal,
        Failure::Net(NetError::Status {
            status: 408 | 425 | 429 | 500..=599,
            ..
        }) => MediaOutcome::Retry,
        _ => MediaOutcome::Permanent,
    }
}

/// Applies the media failure policy; returns `Err` only for a round-ending transport failure.
fn handle_media_failure(
    session: &Arc<Session>,
    id: openpush_client_core::AttachmentId,
    failure: Failure,
) -> BridgeResult<()> {
    match classify(&failure) {
        MediaOutcome::Fatal => match failure {
            Failure::Net(error) => Err(error.into()),
            Failure::Permanent(error) => Err(error),
        },
        MediaOutcome::Retry => {
            record_work_error(session, "media-retry");
            Ok(())
        }
        MediaOutcome::Permanent => {
            let message = match failure {
                Failure::Net(error) => BridgeError::from(error).message,
                Failure::Permanent(error) => error.message,
            };
            session
                .transfer_errors
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id, message);
            Ok(())
        }
    }
}

#[derive(Deserialize)]
struct Reserved {
    attachment_id: String,
}

#[derive(Deserialize)]
struct Finalized {
    attachment_id: String,
    duplicate: bool,
}

pub(crate) async fn upload_one(
    session: &Arc<Session>,
    object: openpush_client_core::CipherObject,
) -> Result<(), Failure> {
    let id = object.attachment_id;
    let body = serde_json::json!({
        "attachment_id": id.to_string(),
        "declared_ciphertext_bytes": object.ciphertext_bytes,
        "declared_ciphertext_sha256": object.ciphertext_sha256,
    });
    let recovering_finalized = match session
        .api
        .post_json::<_, Reserved>("/v1/attachments/reserve", &body)
        .await
    {
        Ok(reserved) => {
            let remote =
                uuid::Uuid::parse_str(&reserved.attachment_id).map_err(|_| NetError::Invalid)?;
            if remote.to_string() != id.to_string() {
                return Err(NetError::Invalid.into());
            }
            false
        }
        // A finalized reservation eventually expires, so an idempotent reserve can conflict.
        // Probe finalize with the same authenticated origin/token; only its typed duplicate proof
        // is enough to recover without uploading bytes again.
        Err(NetError::Status { status: 409, .. }) => true,
        Err(error) => return Err(error.into()),
    };
    let remote = id.to_string();
    if !recovering_finalized {
        let c = client(session);
        let path = blocking(move || c.native_cipher_file(id).map_err(core_error))
            .await
            .map_err(Failure::Permanent)?;
        match session
            .api
            .put_file(
                &format!("/v1/attachments/{remote}/upload"),
                &path,
                object.ciphertext_bytes,
            )
            .await
        {
            Ok(()) => {}
            // Possibly uploaded and finalized before a crash: finalize decides.
            Err(NetError::Status {
                status: 404 | 409, ..
            }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let finalized: Finalized = session
        .api
        .post_empty(&format!("/v1/attachments/{remote}/finalize"))
        .await?;
    let finalized_id =
        uuid::Uuid::parse_str(&finalized.attachment_id).map_err(|_| NetError::Invalid)?;
    if finalized_id.to_string() != id.to_string() || (recovering_finalized && !finalized.duplicate)
    {
        return Err(NetError::Invalid.into());
    }
    session
        .remote_ids
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id, &remote);
    let c = client(session);
    blocking(move || c.mark_attachment_uploaded(id, &remote).map_err(core_error))
        .await
        .map_err(Failure::Permanent)
}

async fn download_one(
    session: &Arc<Session>,
    object: openpush_client_core::CipherObject,
) -> Result<(), Failure> {
    let remote = object.remote_object_id.clone().ok_or(NetError::Invalid)?;
    let remote = uuid::Uuid::parse_str(&remote)
        .map_err(|_| NetError::Invalid)?
        .to_string();
    session
        .remote_ids
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(object.attachment_id, &remote);
    let dir = session.data_dir.join("downloads");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|_| NetError::Local)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|_| NetError::Local)?;
    }
    let target = dir.join(format!("{}.part", object.attachment_id));
    let _ = tokio::fs::remove_file(&target).await;
    session
        .api
        .download_to(
            &format!("/v1/attachments/{remote}"),
            &target,
            object.ciphertext_bytes,
        )
        .await?;
    let (c, id, file) = (client(session), object.attachment_id, target.clone());
    let installed = blocking(move || {
        c.install_downloaded_attachment(id, &file)
            .map_err(core_error)
    })
    .await;
    let _ = tokio::fs::remove_file(&target).await;
    installed.map_err(Failure::Permanent)
}

/// Bounded outbound and media work: uploads (before held MMS envelopes can seal), sealing,
/// outbox upload with acknowledgement only after HTTP acceptance, then verified downloads.
pub async fn work_round(session: &Arc<Session>) -> BridgeResult<()> {
    // Each round reports only its own problems.
    session.set_status(|status| status.work_error = None);
    let mut changed = false;
    let failed: Vec<_> = session
        .transfer_errors
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .keys()
        .copied()
        .collect();
    let c = client(session);
    let uploads = blocking(move || c.pending_uploads().map_err(core_error)).await?;
    for object in uploads
        .into_iter()
        .filter(|o| !failed.contains(&o.attachment_id))
        .take(MEDIA_PER_ROUND)
    {
        let id = object.attachment_id;
        match upload_one(session, object).await {
            Ok(()) => changed = true,
            Err(failure) => {
                handle_media_failure(session, id, failure)?;
                changed = true;
            }
        }
    }
    for _ in 0..MAX_OUTBOX_BATCHES {
        let c = client(session);
        let sealed = blocking(move || match c.seal_pending_batch(MAX_SEAL_BATCH) {
            Ok(count) => Ok(count),
            Err(openpush_client_core::Error::KeysUnavailable) => Ok(0),
            Err(error) => Err(core_error(error)),
        })
        .await?;
        if sealed == 0 {
            break;
        }
    }
    'outbox: for _ in 0..MAX_OUTBOX_BATCHES {
        let c = client(session);
        let batch =
            blocking(move || c.pending_outbox_batch(OUTBOX_BATCH).map_err(core_error)).await?;
        if batch.is_empty() {
            break;
        }
        for envelope in batch {
            let path = match envelope.purpose {
                EnvelopePurpose::Command => "/v1/commands",
                EnvelopePurpose::Event => "/v1/events",
            };
            match session.api.post_discard(path, &envelope).await {
                Ok(()) => {
                    let (c, id) = (client(session), envelope.envelope_id);
                    blocking(move || c.ack_outbox(id).map_err(core_error)).await?;
                    changed = true;
                }
                Err(error) if transport_fatal(&error) => return Err(error.into()),
                Err(_) => {
                    // Typed rejection (for example a retired key epoch). The row stays queued and
                    // is not acknowledged; inbound sync continues.
                    record_work_error(session, "outbox-rejected");
                    break 'outbox;
                }
            }
        }
    }
    let c = client(session);
    let downloads = blocking(move || c.pending_downloads().map_err(core_error)).await?;
    for object in downloads
        .into_iter()
        .filter(|o| !failed.contains(&o.attachment_id))
        .take(MEDIA_PER_ROUND)
    {
        let id = object.attachment_id;
        match download_one(session, object).await {
            Ok(()) => {
                session
                    .previews
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                changed = true;
            }
            Err(failure) => {
                handle_media_failure(session, id, failure)?;
                changed = true;
            }
        }
    }
    // Records journaled while keys were locked (for example history that arrived before an
    // unlock or key-cache import) are applied here; the worker is woken after every unlock.
    drain_apply(session, APPLY_STEPS).await?;
    if changed {
        session.notify();
    }
    Ok(())
}

fn revoke(session: &Arc<Session>) {
    session.set_status(|status| {
        status.revoked = true;
        status.live = false;
        status.error_code = Some("revoked");
    });
    session.cancel.cancel();
    session.notify();
}

fn jitter(base: Duration) -> Duration {
    let mut bytes = [0u8; 2];
    let _ = getrandom::fill(&mut bytes);
    base + base.mul_f64(f64::from(u16::from_le_bytes(bytes)) / f64::from(u16::MAX) / 4.0)
}

/// Worker wait: `delay`, an explicit work request, or cancellation (false when cancelled).
/// Uses its own `Notify`, so a reconnect wait can never consume a work wake-up.
pub(crate) async fn pause_worker(session: &Arc<Session>, delay: Duration) -> bool {
    tokio::select! {
        _ = session.cancel.cancelled() => false,
        _ = sleep(delay) => true,
        _ = session.work_wake.notified() => true,
    }
}

/// Reconnect wait for the live task (woken by unlock, never by ordinary work requests).
pub(crate) async fn pause_live(session: &Arc<Session>, delay: Duration) -> bool {
    tokio::select! {
        _ = session.cancel.cancelled() => false,
        _ = sleep(delay) => true,
        _ = session.reconnect_wake.notified() => true,
    }
}

/// A connection must stay up this long before reconnect backoff resets, so a server that
/// accepts and immediately drops sockets cannot drive a tight reconnect loop.
pub const STABLE_CONNECTION: Duration = Duration::from_secs(60);

pub fn next_backoff(current: Duration, stable: bool) -> Duration {
    if stable {
        Duration::from_secs(1)
    } else {
        (current * 2).min(MAX_BACKOFF)
    }
}

/// Outbound/media worker: runs immediately, on wake hints, and every 15 s.
async fn worker(session: Arc<Session>) {
    let mut ticks = 0u32;
    let mut backoff = Duration::from_secs(1);
    loop {
        if session.cancel.is_cancelled() {
            return;
        }
        if ticks.is_multiple_of(GATEWAY_REFRESH_EVERY) {
            match refresh_gateways(&session).await {
                Err(error) if error.code == "revoked" => return revoke(&session),
                _ => {}
            }
        }
        ticks = ticks.wrapping_add(1);
        let delay = match work_round(&session).await {
            Ok(()) => {
                backoff = Duration::from_secs(1);
                WORK_INTERVAL
            }
            Err(error) if error.code == "revoked" => return revoke(&session),
            Err(_) => {
                backoff = (backoff * 2).min(MAX_BACKOFF);
                jitter(backoff)
            }
        };
        if !pause_worker(&session, delay).await {
            return;
        }
    }
}

enum WsEnd {
    Cancelled,
    Revoked,
    Resync,
    Ended { stable: bool, code: &'static str },
}

fn close_code(frame: Option<&tungstenite::protocol::CloseFrame>) -> Option<u16> {
    frame.map(|frame| u16::from(frame.code))
}

#[derive(Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    envelope: Option<Box<RawValue>>,
}

async fn ws_session(session: &Arc<Session>) -> WsEnd {
    let ended = |code| WsEnd::Ended {
        stable: false,
        code,
    };
    let Ok(url) = websocket_url(session.api.origin()) else {
        return ended("invalid-origin");
    };
    let Ok(mut request) = url.into_client_request() else {
        return ended("invalid-origin");
    };
    let Ok(mut bearer) =
        tungstenite::http::HeaderValue::from_str(&format!("Bearer {}", session.api.bearer()))
    else {
        return ended("credential-invalid");
    };
    bearer.set_sensitive(true);
    request
        .headers_mut()
        .insert(tungstenite::http::header::AUTHORIZATION, bearer);
    let config = WebSocketConfig::default()
        .max_message_size(Some(WS_MAX_MESSAGE))
        .max_frame_size(Some(WS_MAX_MESSAGE));
    // Plain `ws://` is only produced for validated loopback origins; redirects are never followed.
    let connected = tokio::select! {
        _ = session.cancel.cancelled() => return WsEnd::Cancelled,
        result = timeout(WS_CONNECT_TIMEOUT, tokio_tungstenite::connect_async_with_config(request, Some(config), true)) => result,
    };
    let mut socket = match connected {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(tungstenite::Error::Http(response))) if response.status() == 401 => {
            return WsEnd::Revoked
        }
        _ => return ended("live-unavailable"),
    };
    let c = client(session);
    let Ok(cursor) = blocking(move || c.receive_cursor().map_err(core_error)).await else {
        return ended("core");
    };
    let hello = serde_json::json!({"protocol_version": 1, "resume_cursor": cursor.0.to_string()})
        .to_string();
    if socket.send(Message::Text(hello.into())).await.is_err() {
        return ended("live-unavailable");
    }
    // Negotiation: the first data frame must be `ready`.
    let deadline = Instant::now() + WS_READY_TIMEOUT;
    loop {
        let next = tokio::select! {
            _ = session.cancel.cancelled() => { let _ = socket.close(None).await; return WsEnd::Cancelled }
            next = tokio::time::timeout_at(deadline, socket.next()) => next,
        };
        match next {
            Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<Frame>(text.as_str())
            {
                Ok(frame) if frame.kind == "ready" => break,
                Ok(frame) if frame.kind == "resync_required" => return WsEnd::Resync,
                _ => return ended("live-protocol"),
            },
            Ok(Some(Ok(Message::Close(frame)))) => {
                return match close_code(frame.as_ref()) {
                    Some(WS_CLOSE_REVOKED) => WsEnd::Revoked,
                    Some(WS_CLOSE_RESYNC) => WsEnd::Resync,
                    _ => ended("live-closed"),
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => return ended("live-unavailable"),
        }
    }
    session.set_status(|status| {
        status.live = true;
        status.error_code = None;
    });
    session.notify();
    let ready_at = Instant::now();
    let ended = |code| WsEnd::Ended {
        stable: ready_at.elapsed() >= STABLE_CONNECTION,
        code,
    };
    let mut last_frame = Instant::now();
    let mut pinged = false;
    let mut liveness = tokio::time::interval(Duration::from_secs(15));
    loop {
        let next = tokio::select! {
            _ = session.cancel.cancelled() => { let _ = socket.close(None).await; return WsEnd::Cancelled }
            _ = liveness.tick() => {
                let idle = last_frame.elapsed();
                if idle > WS_IDLE_LIMIT {
                    return ended("live-timeout");
                }
                if idle > WS_PING_AFTER && !pinged {
                    pinged = true;
                    if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return ended("live-unavailable");
                    }
                } else if socket.flush().await.is_err() {
                    return ended("live-unavailable");
                }
                continue;
            }
            next = socket.next() => next,
        };
        last_frame = Instant::now();
        pinged = false;
        match next {
            Some(Ok(Message::Text(text))) => {
                let Ok(frame) = serde_json::from_str::<Frame>(text.as_str()) else {
                    return ended("live-protocol");
                };
                match frame.kind.as_str() {
                    "event" => {
                        let (Some(cursor), Some(envelope)) = (frame.cursor, frame.envelope) else {
                            return ended("live-protocol");
                        };
                        if let Err(error) =
                            ingest_page(session, vec![RawRecord { cursor, envelope }]).await
                        {
                            session.set_status(|status| status.error_code = Some(error.code));
                            return ended(error.code);
                        }
                        if drain_apply(session, APPLY_STEPS).await.is_err() {
                            return ended("core");
                        }
                    }
                    "resync_required" => return WsEnd::Resync,
                    _ => {}
                }
            }
            Some(Ok(Message::Close(frame))) => {
                return match close_code(frame.as_ref()) {
                    Some(WS_CLOSE_REVOKED) => WsEnd::Revoked,
                    Some(WS_CLOSE_RESYNC) => WsEnd::Resync,
                    _ => ended("live-closed"),
                }
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return ended("live-unavailable"),
        }
    }
}

/// Live loop: verify credential, HTTP catch-up (including staged snapshot), then WebSocket;
/// reconnect with bounded jittered backoff. Revocation stops all networking for the session.
async fn live(session: Arc<Session>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if session.cancel.is_cancelled() {
            return;
        }
        let round = async {
            refresh_vault(&session).await?;
            replay(&session).await?;
            drain_apply(&session, APPLY_STEPS).await.map(|_| ())
        }
        .await;
        if let Err(error) = round {
            if error.code == "revoked" {
                return revoke(&session);
            }
            session.set_status(|status| {
                status.live = false;
                status.error_code = Some(error.code);
            });
            session.notify();
            backoff = next_backoff(backoff, false);
            if !pause_live(&session, jitter(backoff)).await {
                return;
            }
            continue;
        }
        match ws_session(&session).await {
            WsEnd::Cancelled => return,
            WsEnd::Revoked => return revoke(&session),
            // The next HTTP round imports a snapshot. A short pause bounds any resync ping-pong.
            WsEnd::Resync => {
                if !pause_live(&session, jitter(Duration::from_secs(1))).await {
                    return;
                }
            }
            WsEnd::Ended { stable, code } => {
                session.set_status(|status| {
                    status.live = false;
                    status.error_code = Some(code);
                });
                session.notify();
                backoff = next_backoff(backoff, stable);
                if !pause_live(&session, jitter(backoff)).await {
                    return;
                }
            }
        }
    }
}

/// Starts the worker and live tasks on Tauri's async runtime; both stop when the session's
/// cancellation token fires (session replaced, origin changed, credential revoked, app exit).
pub fn start(session: &Arc<Session>) {
    tauri::async_runtime::spawn(worker(session.clone()));
    tauri::async_runtime::spawn(live(session.clone()));
}
