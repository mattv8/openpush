//! Durable sync surfaces: retained replay, immutable ciphertext snapshots,
//! pending command references and the in-process outbox/retention maintenance.
//!
//! The server only stores and orders opaque envelopes. Snapshot records are
//! `cursor + Envelope`; clients stage-import them and never execute historical
//! commands from a snapshot.

use std::time::Duration;

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use serde_json::{Value, json, value::RawValue};
use sqlx::{PgPool, Postgres, Row, Transaction};
use tokio::sync::broadcast;
use uuid::Uuid;

use super::{ApiError, ApiResult, ApiState, auth, database_unavailable};
use crate::storage::Storage;

pub(super) const MAX_PAGE: u16 = 200;
/// Serialized-envelope byte budget for one replay, snapshot or WebSocket page.
/// The first row is always included, so a page always advances; a single
/// envelope is at most ~1.4 MB of JSON.
pub(super) const PAGE_BYTE_BUDGET: i64 = 8 * 1024 * 1024;
const DEFAULT_PAGE: u16 = 100;
/// Upper bound on transport-log rows removed for one vault per maintenance pass.
const PRUNE_BATCH_ROWS: i64 = 10_000;
/// Upper bound on vaults examined per maintenance pass.
const PRUNE_BATCH_VAULTS: i64 = 100;
/// Clamp for interval arithmetic so configured durations cannot overflow PostgreSQL.
const MAX_INTERVAL_MILLIS: i64 = 10 * 365 * 86_400 * 1_000;

/// Transport timing and retention policy. `router` derives it from the
/// environment; tests and embedders can pass explicit values.
#[derive(Clone, Debug)]
pub struct TransportOptions {
    /// Transport-log replay retention (default 30 days). Older cursors receive `resync_required`.
    pub replay_retention: Duration,
    /// How often a live WebSocket rescans committed rows without any notice.
    pub durable_replay_interval: Duration,
    /// Time allowed for the client hello after the upgrade.
    pub hello_timeout: Duration,
    /// Time allowed for one frame write before the socket is closed for backpressure.
    pub send_timeout: Duration,
    /// Outbox drain cadence.
    pub outbox_drain_interval: Duration,
    /// Maximum outbox jobs claimed or deleted per drain pass.
    pub outbox_batch_size: i64,
    /// How long delivered outbox references are kept before deletion.
    pub outbox_delivered_retention: Duration,
    /// Transport-log pruning cadence.
    pub replay_prune_interval: Duration,
    /// Storage cleanup cadence (expired reservations/copies and queued deletes).
    pub storage_cleanup_interval: Duration,
    /// Maximum rows retired and objects deleted per storage cleanup pass.
    pub storage_cleanup_batch: i64,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            replay_retention: Duration::from_secs(
                u64::from(crate::config::DEFAULT_REPLAY_RETENTION_DAYS) * 86_400,
            ),
            durable_replay_interval: Duration::from_secs(5),
            hello_timeout: Duration::from_secs(10),
            send_timeout: Duration::from_secs(10),
            outbox_drain_interval: Duration::from_secs(1),
            outbox_batch_size: 256,
            outbox_delivered_retention: Duration::from_secs(3_600),
            replay_prune_interval: Duration::from_secs(300),
            storage_cleanup_interval: Duration::from_secs(30),
            storage_cleanup_batch: 16,
        }
    }
}

fn interval_millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis())
        .unwrap_or(i64::MAX)
        .min(MAX_INTERVAL_MILLIS)
}

/// Parses a decimal cursor string. Cursors are non-negative JSON strings.
pub(super) fn parse_cursor(value: &str) -> ApiResult<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|cursor| *cursor >= 0)
        .ok_or(ApiError(StatusCode::BAD_REQUEST, "invalid_cursor"))
}

fn page_limit(limit: Option<u16>) -> i64 {
    i64::from(limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE))
}

/// A cursor the transport log cannot serve. Clients recover with a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Resync {
    pub reason: ResyncReason,
    pub high_water: i64,
    pub replay_floor: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResyncReason {
    /// The cursor is older than retained transport history.
    CursorExpired,
    /// The cursor is beyond current server state, for example after a restore/rollback.
    CursorAhead,
}

impl ResyncReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::CursorExpired => "cursor_expired",
            Self::CursorAhead => "cursor_ahead",
        }
    }
}

impl Resync {
    fn classify(cursor: i64, high_water: i64, replay_floor: i64) -> Option<Self> {
        let reason = if cursor > high_water {
            ResyncReason::CursorAhead
        } else if cursor < replay_floor {
            ResyncReason::CursorExpired
        } else {
            return None;
        };
        Some(Self {
            reason,
            high_water,
            replay_floor,
        })
    }

    fn fields(&self) -> serde_json::Map<String, Value> {
        let mut fields = serde_json::Map::new();
        fields.insert("reason".into(), json!(self.reason.as_str()));
        fields.insert(
            "high_water_cursor".into(),
            json!(self.high_water.to_string()),
        );
        fields.insert(
            "replay_floor_cursor".into(),
            json!(self.replay_floor.to_string()),
        );
        fields.insert("snapshot_path".into(), json!("/v1/snapshot"));
        fields
    }

    /// WebSocket frame sent immediately before close code 4409.
    pub(super) fn ws_frame(&self) -> String {
        let mut fields = self.fields();
        fields.insert("type".into(), json!("resync_required"));
        Value::Object(fields).to_string()
    }
}

impl IntoResponse for Resync {
    fn into_response(self) -> Response {
        let mut fields = self.fields();
        fields.insert("code".into(), json!("resync_required"));
        (StatusCode::CONFLICT, Json(Value::Object(fields))).into_response()
    }
}

/// Current vault watermarks read inside the caller's transaction.
struct Watermarks {
    high_water: i64,
    replay_floor: i64,
}

async fn watermarks(
    tx: &mut Transaction<'_, Postgres>,
    vault: Uuid,
) -> Result<Watermarks, sqlx::Error> {
    let row = sqlx::query("SELECT next_cursor,replay_floor_cursor FROM vaults WHERE vault_id=$1")
        .bind(vault)
        .fetch_one(&mut **tx)
        .await?;
    Ok(Watermarks {
        high_water: row.get("next_cursor"),
        replay_floor: row.get("replay_floor_cursor"),
    })
}

async fn read_only_snapshot(db: &PgPool) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// One committed envelope, kept as the database's serialized JSON so pages
/// are moved into the response without building or cloning a `Value` tree.
pub(super) struct CursorRecord {
    pub cursor: i64,
    pub envelope: Box<RawValue>,
}

impl Serialize for CursorRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut record = serializer.serialize_struct("CursorRecord", 2)?;
        record.serialize_field("cursor", &self.cursor.to_string())?;
        record.serialize_field("envelope", &self.envelope)?;
        record.end()
    }
}

/// WebSocket `event` frame.
pub(super) struct EventFrame(CursorRecord);

impl EventFrame {
    pub(super) fn new(record: CursorRecord) -> Self {
        Self(record)
    }
}

impl Serialize for EventFrame {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut frame = serializer.serialize_struct("EventFrame", 3)?;
        frame.serialize_field("type", "event")?;
        frame.serialize_field("cursor", &self.0.cursor.to_string())?;
        frame.serialize_field("envelope", &self.0.envelope)?;
        frame.end()
    }
}

/// Source table for a budgeted page. Both are trusted constants.
#[derive(Clone, Copy)]
enum PageSource {
    TransportLog,
    ImmutableRecords,
}

/// Reads rows in `(after, upper]` in cursor order, at most `limit` rows and at
/// most `PAGE_BYTE_BUDGET` serialized bytes (always at least one row). The
/// budget is applied in SQL, so oversized pages are never transferred.
async fn fetch_budgeted(
    tx: &mut Transaction<'_, Postgres>,
    source: PageSource,
    vault: Uuid,
    after: i64,
    upper: i64,
    limit: i64,
) -> Result<Vec<CursorRecord>, sqlx::Error> {
    let table = match source {
        PageSource::TransportLog => "event_log",
        PageSource::ImmutableRecords => "encrypted_records",
    };
    let rows = sqlx::query(&format!("SELECT cursor,body FROM (SELECT cursor,body,row_number() OVER w AS position,sum(octet_length(body)) OVER w AS running FROM (SELECT cursor,envelope::text AS body FROM {table} WHERE vault_id=$1 AND cursor>$2 AND cursor<=$3 ORDER BY cursor LIMIT $4) page WINDOW w AS (ORDER BY cursor)) sized WHERE position=1 OR running<=$5 ORDER BY cursor"))
        .bind(vault)
        .bind(after)
        .bind(upper)
        .bind(limit)
        .bind(PAGE_BYTE_BUDGET)
        .fetch_all(&mut **tx)
        .await?;
    rows.into_iter()
        .map(|row| {
            let body: String = row.get("body");
            Ok(CursorRecord {
                cursor: row.get("cursor"),
                envelope: RawValue::from_string(body)
                    .map_err(|error| sqlx::Error::Decode(Box::new(error)))?,
            })
        })
        .collect()
}

/// Cursor to request next, or `None` when `records` reached `upper`.
fn next_after(records: &[CursorRecord], upper: i64) -> Option<String> {
    records
        .last()
        .map(|record| record.cursor)
        .filter(|last| *last < upper)
        .map(|last| last.to_string())
}

pub(super) enum ReplayPage {
    Rows {
        high_water: i64,
        replay_floor: i64,
        rows: Vec<CursorRecord>,
    },
    Resync(Resync),
}

/// Reads one budgeted replay page after `after` from a single consistent
/// snapshot of the vault watermarks and transport log. Pruning moves the floor
/// and deletes rows atomically, so a page never silently skips a pruned cursor.
pub(super) async fn replay_page(
    db: &PgPool,
    vault: Uuid,
    after: i64,
    limit: i64,
) -> Result<ReplayPage, sqlx::Error> {
    let mut tx = read_only_snapshot(db).await?;
    let marks = watermarks(&mut tx, vault).await?;
    if let Some(resync) = Resync::classify(after, marks.high_water, marks.replay_floor) {
        tx.commit().await?;
        return Ok(ReplayPage::Resync(resync));
    }
    let rows = fetch_budgeted(
        &mut tx,
        PageSource::TransportLog,
        vault,
        after,
        marks.high_water,
        limit,
    )
    .await?;
    tx.commit().await?;
    Ok(ReplayPage::Rows {
        high_water: marks.high_water,
        replay_floor: marks.replay_floor,
        rows,
    })
}

/// Watermarks used by the WebSocket handshake before replay begins.
pub(super) async fn position(
    db: &PgPool,
    vault: Uuid,
    cursor: i64,
) -> Result<Result<(i64, i64), Resync>, sqlx::Error> {
    let mut tx = read_only_snapshot(db).await?;
    let marks = watermarks(&mut tx, vault).await?;
    tx.commit().await?;
    Ok(
        match Resync::classify(cursor, marks.high_water, marks.replay_floor) {
            Some(resync) => Err(resync),
            None => Ok((marks.high_water, marks.replay_floor)),
        },
    )
}

#[derive(Serialize)]
struct EventsPage {
    high_water_cursor: String,
    replay_floor_cursor: String,
    next_after: Option<String>,
    events: Vec<CursorRecord>,
}

#[derive(Serialize)]
struct SnapshotPage {
    high_water_cursor: String,
    next_after: Option<String>,
    records: Vec<CursorRecord>,
}

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    after: Option<String>,
    limit: Option<u16>,
}

/// `GET /v1/events?after=&limit=` — retained transport replay.
pub(super) async fn events(
    State(s): State<ApiState>,
    h: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Result<Response, ApiError> {
    let p = auth(&s.db, &h).await?;
    let after = parse_cursor(q.after.as_deref().unwrap_or("0"))?;
    match replay_page(&s.db, p.vault, after, page_limit(q.limit))
        .await
        .map_err(|error| database_unavailable(&error, "events_replay"))?
    {
        ReplayPage::Resync(resync) => Ok(resync.into_response()),
        ReplayPage::Rows {
            high_water,
            replay_floor,
            rows,
        } => Ok(Json(EventsPage {
            high_water_cursor: high_water.to_string(),
            replay_floor_cursor: replay_floor.to_string(),
            next_after: next_after(&rows, high_water),
            events: rows,
        })
        .into_response()),
    }
}

/// `GET /v1/snapshot` — fixes a cut for immutable ciphertext record paging.
pub(super) async fn snapshot_start(
    State(s): State<ApiState>,
    h: HeaderMap,
) -> ApiResult<Json<Value>> {
    let p = auth(&s.db, &h).await?;
    let read = async {
        let mut tx = read_only_snapshot(&s.db).await?;
        let marks = watermarks(&mut tx, p.vault).await?;
        let row = sqlx::query("SELECT key_epoch,profile_fingerprint FROM vaults WHERE vault_id=$1")
            .bind(p.vault)
            .fetch_one(&mut *tx)
            .await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM encrypted_records WHERE vault_id=$1 AND cursor<=$2",
        )
        .bind(p.vault)
        .bind(marks.high_water)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>((marks, row, count))
    };
    let (marks, row, count) = read
        .await
        .map_err(|error| database_unavailable(&error, "snapshot_start"))?;
    Ok(Json(json!({
        "snapshot_version": 1,
        "vault_id": p.vault,
        "high_water_cursor": marks.high_water.to_string(),
        "record_count": count.to_string(),
        "replay_floor_cursor": marks.replay_floor.to_string(),
        "key_epoch": row.get::<i32, _>("key_epoch"),
        "profile_fingerprint": row.get::<String, _>("profile_fingerprint"),
        "max_page_size": MAX_PAGE,
    })))
}

#[derive(Deserialize)]
pub(super) struct SnapshotPageQuery {
    high_water: String,
    after: Option<String>,
    limit: Option<u16>,
}

/// `GET /v1/snapshot/records?high_water=&after=&limit=` — one stable page under a fixed cut.
///
/// Records are immutable and cursors are allocated in commit order under the
/// vault lock, so every page at or below `high_water` is stable without holding
/// a server-side snapshot session.
pub(super) async fn snapshot_records(
    State(s): State<ApiState>,
    h: HeaderMap,
    Query(q): Query<SnapshotPageQuery>,
) -> Result<Response, ApiError> {
    let p = auth(&s.db, &h).await?;
    let high_water = parse_cursor(&q.high_water)?;
    let after = parse_cursor(q.after.as_deref().unwrap_or("0"))?;
    if after > high_water {
        return Err(ApiError(StatusCode::BAD_REQUEST, "invalid_cursor"));
    }
    let limit = page_limit(q.limit);
    let read = async {
        let mut tx = read_only_snapshot(&s.db).await?;
        let marks = watermarks(&mut tx, p.vault).await?;
        if high_water > marks.high_water {
            tx.commit().await?;
            return Ok(Err(Resync {
                reason: ResyncReason::CursorAhead,
                high_water: marks.high_water,
                replay_floor: marks.replay_floor,
            }));
        }
        let rows = fetch_budgeted(
            &mut tx,
            PageSource::ImmutableRecords,
            p.vault,
            after,
            high_water,
            limit,
        )
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(Ok(rows))
    };
    let rows = match read
        .await
        .map_err(|error| database_unavailable(&error, "snapshot_records"))?
    {
        Ok(rows) => rows,
        Err(resync) => return Ok(resync.into_response()),
    };
    Ok(Json(SnapshotPage {
        high_water_cursor: high_water.to_string(),
        next_after: next_after(&rows, high_water),
        records: rows,
    })
    .into_response())
}

#[derive(Deserialize)]
pub(super) struct PendingQuery {
    after: Option<String>,
    limit: Option<u16>,
}

/// `GET /v1/commands/pending` — references to commands without a receipt from
/// their original target gateway. Gateways see only commands targeted to them.
/// This is read-only reconciliation data; it never retargets or re-identifies.
pub(super) async fn pending_commands(
    State(s): State<ApiState>,
    h: HeaderMap,
    Query(q): Query<PendingQuery>,
) -> ApiResult<Json<Value>> {
    let p = auth(&s.db, &h).await?;
    let after = parse_cursor(q.after.as_deref().unwrap_or("0"))?;
    let gateway_filter = (p.role == "gateway").then_some(p.device);
    let rows = sqlx::query("SELECT c.command_id,c.producer_device_id,c.gateway_device_id,c.cursor FROM commands c WHERE c.vault_id=$1 AND ($2::uuid IS NULL OR c.gateway_device_id=$2) AND c.cursor>$3 AND NOT EXISTS (SELECT 1 FROM command_receipts r WHERE r.vault_id=c.vault_id AND r.command_id=c.command_id AND r.gateway_device_id=c.gateway_device_id) ORDER BY c.cursor LIMIT $4")
        .bind(p.vault)
        .bind(gateway_filter)
        .bind(after)
        .bind(page_limit(q.limit))
        .fetch_all(&s.db)
        .await
        .map_err(|error| database_unavailable(&error, "pending_commands"))?;
    Ok(Json(json!({
        "commands": rows.into_iter().map(|row| json!({
            "command_id": row.get::<Uuid, _>("command_id"),
            "producer_device_id": row.get::<Uuid, _>("producer_device_id"),
            "gateway_device_id": row.get::<Uuid, _>("gateway_device_id"),
            "cursor": row.get::<i64, _>("cursor").to_string(),
        })).collect::<Vec<_>>()
    })))
}

/// Result of one outbox drain pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainStats {
    pub published: u64,
    pub deleted: u64,
}

/// Claims due outbox references, publishes their committed cursor as an
/// in-process latency hint, marks them delivered, and deletes delivered
/// references older than the retention. Delivery is at-least-once: a crash
/// before commit republishes, which receivers ignore by cursor. Hints are never
/// authoritative; sockets also rescan committed rows on a timer.
pub async fn drain_outbox(
    db: &PgPool,
    hints: &broadcast::Sender<(Uuid, i64)>,
    batch_size: i64,
    delivered_retention: Duration,
) -> Result<DrainStats, sqlx::Error> {
    let batch_size = batch_size.max(1);
    let mut tx = db.begin().await?;
    let due = sqlx::query("SELECT vault_id,cursor,kind FROM outbox_jobs WHERE delivered_at IS NULL AND available_at<=now() ORDER BY available_at,vault_id,cursor LIMIT $1 FOR UPDATE SKIP LOCKED")
        .bind(batch_size)
        .fetch_all(&mut *tx)
        .await?;
    let mut vaults = Vec::with_capacity(due.len());
    let mut cursors = Vec::with_capacity(due.len());
    let mut kinds = Vec::with_capacity(due.len());
    for row in &due {
        vaults.push(row.get::<Uuid, _>("vault_id"));
        cursors.push(row.get::<i64, _>("cursor"));
        kinds.push(row.get::<String, _>("kind"));
    }
    if !due.is_empty() {
        sqlx::query("UPDATE outbox_jobs o SET delivered_at=now(),attempts=o.attempts+1 FROM UNNEST($1::uuid[],$2::bigint[],$3::text[]) AS d(vault_id,cursor,kind) WHERE o.vault_id=d.vault_id AND o.cursor=d.cursor AND o.kind=d.kind")
            .bind(&vaults)
            .bind(&cursors)
            .bind(&kinds)
            .execute(&mut *tx)
            .await?;
    }
    // Publish while the claim is held; if commit fails the jobs are retried.
    for (vault, cursor) in vaults.iter().zip(&cursors) {
        let _ = hints.send((*vault, *cursor));
    }
    tx.commit().await?;
    let deleted = sqlx::query("DELETE FROM outbox_jobs WHERE ctid IN (SELECT ctid FROM outbox_jobs WHERE delivered_at IS NOT NULL AND delivered_at < now() - ($1::bigint * interval '1 millisecond') LIMIT $2)")
        .bind(interval_millis(delivered_retention))
        .bind(batch_size)
        .execute(db)
        .await?
        .rows_affected();
    Ok(DrainStats {
        published: cursors.len() as u64,
        deleted,
    })
}

/// Prunes an expired contiguous prefix of each vault's transport log and
/// advances `replay_floor_cursor` in the same transaction. Immutable
/// `encrypted_records` are never touched. Returns the number of rows removed.
pub async fn prune_replay_log(db: &PgPool, retention: Duration) -> Result<u64, sqlx::Error> {
    let retention = interval_millis(retention);
    let vaults: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT vault_id FROM event_log WHERE created_at < now() - ($1::bigint * interval '1 millisecond') LIMIT $2")
        .bind(retention)
        .bind(PRUNE_BATCH_VAULTS)
        .fetch_all(db)
        .await?;
    let mut removed = 0;
    for vault in vaults {
        let mut tx = db.begin().await?;
        // Ingest allocates cursors under this lock, so no cursor can appear
        // below the computed cut while it is held.
        let floor: i64 = sqlx::query_scalar(
            "SELECT replay_floor_cursor FROM vaults WHERE vault_id=$1 FOR UPDATE",
        )
        .bind(vault)
        .fetch_one(&mut *tx)
        .await?;
        let bounds = sqlx::query("SELECT (SELECT MIN(cursor) FROM event_log WHERE vault_id=$1 AND created_at >= now() - ($2::bigint * interval '1 millisecond')) AS youngest, (SELECT MAX(cursor) FROM event_log WHERE vault_id=$1) AS newest")
            .bind(vault)
            .bind(retention)
            .fetch_one(&mut *tx)
            .await?;
        let youngest: Option<i64> = bounds.get("youngest");
        let newest: Option<i64> = bounds.get("newest");
        let Some(newest) = newest else {
            tx.commit().await?;
            continue;
        };
        // Every row below the youngest retained row is expired.
        let cut = youngest
            .map_or(newest, |cursor| cursor - 1)
            .min(floor.saturating_add(PRUNE_BATCH_ROWS));
        if cut <= floor {
            tx.commit().await?;
            continue;
        }
        removed += sqlx::query("DELETE FROM event_log WHERE vault_id=$1 AND cursor<=$2")
            .bind(vault)
            .bind(cut)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        sqlx::query("UPDATE vaults SET replay_floor_cursor=$2 WHERE vault_id=$1")
            .bind(vault)
            .bind(cut)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(removed)
}

/// Starts the bounded in-process maintenance loop. It stops when the router's
/// hint sender is dropped or the pool closes; there is no separate worker.
pub(super) fn spawn_maintenance(
    db: PgPool,
    hints: &broadcast::Sender<(Uuid, i64)>,
    storage: Option<Storage>,
    options: TransportOptions,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("no tokio runtime; outbox drain and replay pruning are disabled");
        return;
    };
    let weak = hints.downgrade();
    runtime.spawn(async move {
        let mut drain = tokio::time::interval(options.outbox_drain_interval);
        drain.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut prune = tokio::time::interval(options.replay_prune_interval);
        prune.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut cleanup = tokio::time::interval(options.storage_cleanup_interval);
        cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = drain.tick() => {
                    let Some(hints) = weak.upgrade() else { return };
                    if db.is_closed() { return; }
                    if let Err(error) = drain_outbox(&db, &hints, options.outbox_batch_size, options.outbox_delivered_retention).await {
                        tracing::warn!(error_kind = %error, "outbox drain failed");
                    }
                }
                _ = prune.tick() => {
                    if weak.strong_count() == 0 || db.is_closed() { return; }
                    match prune_replay_log(&db, options.replay_retention).await {
                        Ok(0) => {}
                        Ok(rows) => tracing::info!(rows, "pruned expired replay log rows"),
                        Err(error) => tracing::warn!(error_kind = %error, "replay pruning failed"),
                    }
                }
                _ = cleanup.tick(), if storage.is_some() => {
                    if weak.strong_count() == 0 || db.is_closed() { return; }
                    let Some(store) = &storage else { continue };
                    if let Err(error) = super::attachments::storage_maintenance(&db, store, options.storage_cleanup_batch).await {
                        tracing::warn!(error_kind = %error, "storage cleanup failed");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resync_classifies_expired_and_ahead_cursors() {
        assert_eq!(Resync::classify(5, 10, 5), None);
        assert_eq!(Resync::classify(10, 10, 0), None);
        assert_eq!(
            Resync::classify(4, 10, 5).map(|r| r.reason),
            Some(ResyncReason::CursorExpired)
        );
        assert_eq!(
            Resync::classify(11, 10, 0).map(|r| r.reason),
            Some(ResyncReason::CursorAhead)
        );
    }

    #[test]
    fn cursor_and_page_parsing_are_bounded() {
        assert_eq!(parse_cursor("0").unwrap(), 0);
        assert!(parse_cursor("-1").is_err());
        assert!(parse_cursor("1.5").is_err());
        assert_eq!(page_limit(Some(0)), 1);
        assert_eq!(page_limit(Some(5_000)), i64::from(MAX_PAGE));
        assert_eq!(page_limit(None), i64::from(DEFAULT_PAGE));
        assert_eq!(interval_millis(Duration::MAX), MAX_INTERVAL_MILLIS);
    }
}
