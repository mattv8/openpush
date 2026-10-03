//! Generated native facade for the shared SQLCipher client. Native hosts own
//! transport, scheduling, carrier effects, and secure persistence.

use peppy_client_core::{
    AttachmentInfo, AttachmentState, Captured, CipherObject, Client, ClientConfig,
    ComposeDraftUpdate, DatabaseKey, DeviceId, Error as CoreError, IncomingSms, KeyProfile,
    MAX_SEAL_BATCH, Message, MmsAcquisitionInput, MmsAcquisitionState, MmsSource, NativeKeyCache,
    NotificationCapture, NotificationCaptureOutcome, NotificationTarget, PermitBlock,
    PermitDecision, ReceivedCommand, SendResult, Transport, VaultCheckHeader, VaultId,
};
use std::{
    fmt,
    str::FromStr,
    sync::{Arc, Mutex},
};
use zeroize::Zeroize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CapabilityState {
    Available,
    PermissionRequired,
    ApprovalRequired,
    RegionRestricted,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CapabilityDiagnostic {
    pub name: String,
    pub state: CapabilityState,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GatewayHealth {
    pub enrollment_state: String,
    pub diagnostics: Vec<CapabilityDiagnostic>,
}

#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeOpenConfig {
    pub database_path: String,
    pub vault_id: String,
    pub device_id: String,
    /// Exactly 32 random bytes supplied by native secure storage.
    pub database_key: Vec<u8>,
}

impl fmt::Debug for NativeOpenConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeOpenConfig")
            .field("database_path", &self.database_path)
            .field("vault_id", &self.vault_id)
            .field("device_id", &self.device_id)
            .field("database_key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeVaultMaterial {
    pub profile_json: String,
    pub header_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeIncomingSms {
    pub conversation_id: Option<String>,
    pub sender_address: String,
    pub body: String,
    pub provider_message_id: Option<String>,
    pub imported: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeMmsSource {
    pub source_generation: String,
    pub subscription_id: String,
    pub provider_message_id: String,
    pub provider_thread_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeMmsAcquisitionInput {
    pub source: NativeMmsSource,
    pub incoming: bool,
    pub sender_address: Option<String>,
    pub recipients: Vec<String>,
    pub subject: Option<String>,
    pub body: String,
    pub imported: bool,
    pub observed_at_ms: i64,
    pub transaction_id: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NativeMmsAcquisitionState {
    Pending,
    Blocked,
    Unavailable,
    Complete,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeMmsAcquisition {
    pub acquisition_id: String,
    pub conversation_id: String,
    pub input: NativeMmsAcquisitionInput,
    pub state: NativeMmsAcquisitionState,
    pub reason: Option<String>,
    pub attachment_ids: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeMmsAcquisitionPart {
    pub provider_part_id: String,
    pub attachment_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeMmsReplyContext {
    pub recipients: Vec<String>,
    pub blocked_reason: Option<String>,
    pub subject: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeCaptured {
    pub message_id: String,
    pub conversation_id: String,
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeNotificationCapture {
    pub notification_key: String,
    pub instance: String,
    pub package_name: String,
    pub app_name: String,
    pub title: String,
    pub text: String,
    pub category: Option<String>,
    pub posted_at: i64,
    pub dismissible: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeNotificationTarget {
    pub source_device_id: String,
    pub notification_key: String,
    pub lifetime: String,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeMirroredNotification {
    pub target: NativeNotificationTarget,
    pub package_name: String,
    pub app_name: String,
    pub title: String,
    pub text: String,
    pub category: Option<String>,
    pub posted_at: i64,
    pub dismissible: bool,
    pub seen: bool,
    pub dismissal_pending: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeAppFilter {
    pub source_device_id: String,
    pub package_name: String,
    pub app_name: String,
    pub muted: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeNotificationSnapshot {
    pub notifications: Vec<NativeMirroredNotification>,
    pub app_filters: Vec<NativeAppFilter>,
}
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeNotificationDismissal {
    pub id: String,
    pub target: NativeNotificationTarget,
    pub instance: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NativeNotificationCaptureOutcome {
    Captured,
    Duplicate,
    FilteredOut,
    DroppedLocked,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeConversation {
    pub conversation_id: String,
    pub unread_count: u64,
}

/// Sanitized message presentation data. Database keys, passphrases, purpose keys,
/// and envelope encryption keys are never returned through this record.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeMessage {
    pub message_id: String,
    pub conversation_id: String,
    pub sender_address: Option<String>,
    pub recipients: Vec<String>,
    pub body: String,
    pub subject: Option<String>,
    pub transport: String,
    pub attachment_ids: Vec<String>,
    pub incoming: bool,
    pub seen: bool,
    pub send_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeComposeDraft {
    pub draft_id: String,
    pub conversation_id: String,
    pub text: String,
    pub recipients: Vec<String>,
    pub attachment_ids: Vec<String>,
    /// Opaque `GatewayRoute` JSON owned by Rust's protocol crate.
    pub route_json: Option<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeComposeDraftUpdate {
    pub text: String,
    pub recipients: Vec<String>,
    pub attachment_ids: Vec<String>,
    pub route_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeQueuedSend {
    pub message_id: String,
    pub command_id: String,
    pub envelope_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NativeIngestState {
    Journaled,
    Duplicate,
    Quarantined,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeIngestResult {
    pub state: NativeIngestState,
    pub quarantine_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NativePermitState {
    Permit,
    AlreadyAttempted,
    NotReceived,
    StaleEpoch,
    EpochNotActive,
    MediaUnavailable,
    Historical,
    RestoreGuarded,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeCarrierCommand {
    pub command_id: String,
    pub subscription_id: String,
    pub message: NativeMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativePermit {
    pub state: NativePermitState,
    /// Present only when state is Permit. Only that state authorizes a carrier effect.
    pub command: Option<NativeCarrierCommand>,
    pub existing_send_state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NativeSendResult {
    Submitted,
    Sent,
    Delivered,
    FailedBeforeSubmission,
    FailedConfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NativeAttachmentState {
    PendingUpload,
    Uploaded,
    PendingDownload,
    Available,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeAttachmentInfo {
    pub attachment_id: String,
    pub media_type: String,
    pub display_name: String,
    pub plaintext_bytes: u64,
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: String,
    pub state: NativeAttachmentState,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeCipherObject {
    pub attachment_id: String,
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: String,
    pub remote_object_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SyncStepState {
    Completed,
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SyncStepResult {
    pub state: SyncStepState,
    pub effects_started: u32,
}

/// A bounded native work result. After publishing a snapshot, hosts must keep
/// calling `apply_pending` until `snapshot_remaining` is zero; `applied == 0`
/// alone does not mean published history has drained.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeApplyReport {
    pub applied: u64,
    pub quarantined: u64,
    pub waiting_for_keys: u64,
    pub drained: u64,
    pub snapshot_remaining: u64,
    pub superseded: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NativeSnapshotPurpose {
    Resync,
    Restore,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeSnapshotProgress {
    pub generation: u64,
    pub high_water: String,
    pub expected_records: u64,
    pub received_records: u64,
    pub last_cursor: String,
    pub server_compaction_generation: Option<String>,
}

/// State of the newest authoritative (server compaction) snapshot projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NativeSnapshotProjectionState {
    Draining,
    Staging,
    Promoted,
    /// Not promoted; live state was left unchanged. Fetch a new snapshot after `reason`.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeSnapshotProjectionStatus {
    pub generation: u64,
    pub high_water: String,
    pub state: NativeSnapshotProjectionState,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeSnapshotReport {
    pub journaled: u64,
    pub duplicate: u64,
    pub quarantined: u64,
    pub receive_cursor: String,
}

/// Opaque server snapshot JSON. Rust canonicalizes it exactly as `ingest_raw`,
/// preserving malformed records for quarantine during the later bounded drain.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NativeRawSnapshotRecord {
    pub cursor: String,
    pub envelope_json: Vec<u8>,
}

/// Stable, non-secret error codes for generated hosts.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MobileBindingsError {
    #[error("the native client handle has been closed")]
    Closed,
    #[error("invalid native request")]
    InvalidRequest,
    #[error("database key must be exactly 32 bytes")]
    InvalidDatabaseKey,
    #[error("database key does not open this database")]
    WrongDatabaseKey,
    #[error("database unavailable or corrupt")]
    Database,
    #[error("unsupported local schema version")]
    UnsupportedSchema,
    #[error("database belongs to a different vault or device")]
    IdentityMismatch,
    #[error("passphrase does not open this vault profile")]
    WrongPassphrase,
    #[error("invalid passphrase or profile")]
    InvalidProfile,
    #[error("cryptographic operation failed")]
    Crypto,
    #[error("cursor conflict")]
    Conflict,
    #[error("record not found")]
    NotFound,
    #[error("stale draft")]
    StaleDraft,
    #[error("native key cache was malformed or altered")]
    InvalidKeyCache,
    #[error("keys for this epoch are unavailable")]
    KeysUnavailable,
    #[error("attachment media is invalid or unavailable")]
    InvalidMedia,
    #[error("native media storage is unavailable")]
    Storage,
    #[error("MMS acquisition limit reached")]
    MmsAcquisitionLimit,
    #[error("MMS pending media quota exceeded")]
    MmsMediaQuota,
}

impl From<CoreError> for MobileBindingsError {
    fn from(value: CoreError) -> Self {
        match value {
            CoreError::InvalidDatabaseKey => Self::InvalidDatabaseKey,
            CoreError::WrongDatabaseKey => Self::WrongDatabaseKey,
            CoreError::Database => Self::Database,
            CoreError::UnsupportedSchema => Self::UnsupportedSchema,
            CoreError::IdentityMismatch => Self::IdentityMismatch,
            CoreError::WrongPassphrase => Self::WrongPassphrase,
            CoreError::InvalidPassphrase | CoreError::InvalidProfile => Self::InvalidProfile,
            CoreError::Crypto => Self::Crypto,
            CoreError::InvalidRequest(_)
            | CoreError::InvalidCursor
            | CoreError::NoAttempt
            | CoreError::IllegalTransition { .. } => Self::InvalidRequest,
            CoreError::Conflict => Self::Conflict,
            CoreError::NotFound => Self::NotFound,
            CoreError::StaleDraft { .. } => Self::StaleDraft,
            CoreError::InvalidKeyCache => Self::InvalidKeyCache,
            CoreError::KeysUnavailable => Self::KeysUnavailable,
            CoreError::InvalidMedia => Self::InvalidMedia,
            CoreError::Storage => Self::Storage,
            CoreError::SnapshotMismatch => Self::Conflict,
            CoreError::MmsAcquisitionLimit => Self::MmsAcquisitionLimit,
            CoreError::MmsMediaQuota => Self::MmsMediaQuota,
        }
    }
}

#[derive(uniffi::Object)]
pub struct NativeClient {
    client: Mutex<Option<Client>>,
}

/// Owns a core-created temporary plaintext file. It is deleted when this handle
/// is disposed or dropped; callers must never forward its path to a web view.
#[derive(uniffi::Object)]
pub struct NativePlaintextHandle {
    file: Mutex<Option<peppy_client_core::NativePlaintextFile>>,
}

fn parse_id<T: FromStr>(value: &str) -> Result<T, MobileBindingsError> {
    value
        .parse()
        .map_err(|_| MobileBindingsError::InvalidRequest)
}

fn parse_cursor(value: String) -> Result<peppy_client_core::Cursor, MobileBindingsError> {
    value
        .parse::<u64>()
        .map(peppy_client_core::Cursor)
        .map_err(|_| MobileBindingsError::InvalidRequest)
}

fn parse_canonical_u64(value: &str) -> Result<u64, MobileBindingsError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| MobileBindingsError::InvalidRequest)?;
    if parsed.to_string() != value {
        return Err(MobileBindingsError::InvalidRequest);
    }
    Ok(parsed)
}

fn notification_target(value: NativeNotificationTarget) -> NotificationTarget {
    NotificationTarget {
        source_device_id: value.source_device_id,
        notification_key: value.notification_key,
        lifetime: value.lifetime,
    }
}
fn notification_target_view(value: NotificationTarget) -> NativeNotificationTarget {
    NativeNotificationTarget {
        source_device_id: value.source_device_id,
        notification_key: value.notification_key,
        lifetime: value.lifetime,
    }
}
fn notification_snapshot_view(
    value: peppy_client_core::NotificationSnapshot,
) -> NativeNotificationSnapshot {
    NativeNotificationSnapshot {
        notifications: value
            .notifications
            .into_iter()
            .map(|n| NativeMirroredNotification {
                target: notification_target_view(n.target),
                package_name: n.package_name,
                app_name: n.app_name,
                title: n.title,
                text: n.text,
                category: n.category,
                posted_at: n.posted_at,
                dismissible: n.dismissible,
                seen: n.seen,
                dismissal_pending: n.dismissal_pending,
            })
            .collect(),
        app_filters: value
            .app_filters
            .into_iter()
            .map(|f| NativeAppFilter {
                source_device_id: f.source_device_id,
                package_name: f.package_name,
                app_name: f.app_name,
                muted: f.muted,
            })
            .collect(),
    }
}

fn snapshot_progress_view(progress: peppy_client_core::SnapshotProgress) -> NativeSnapshotProgress {
    NativeSnapshotProgress {
        generation: progress.generation,
        high_water: progress.high_water.0.to_string(),
        expected_records: progress.expected_records,
        received_records: progress.received_records,
        last_cursor: progress.last_cursor.0.to_string(),
        server_compaction_generation: progress.server_compaction_generation.map(|v| v.to_string()),
    }
}

fn message_view(message: Message) -> NativeMessage {
    NativeMessage {
        message_id: message.payload.record.message_id.to_string(),
        conversation_id: message.payload.record.conversation_id.to_string(),
        sender_address: message.payload.sender_address,
        recipients: message.payload.recipients,
        body: message.payload.body,
        subject: message.payload.subject,
        transport: transport_name(message.payload.transport).into(),
        attachment_ids: message
            .payload
            .record
            .attachments
            .into_iter()
            .map(|a| a.attachment_id.to_string())
            .collect(),
        incoming: matches!(
            message.payload.direction,
            peppy_client_core::Direction::Incoming
        ),
        seen: message.seen,
        send_state: message.send_state.map(|state| format!("{state:?}")),
    }
}

fn payload_view(payload: peppy_client_core::MessagePayload) -> NativeMessage {
    NativeMessage {
        message_id: payload.record.message_id.to_string(),
        conversation_id: payload.record.conversation_id.to_string(),
        sender_address: payload.sender_address,
        recipients: payload.recipients,
        body: payload.body,
        subject: payload.subject,
        transport: transport_name(payload.transport).into(),
        attachment_ids: payload
            .record
            .attachments
            .into_iter()
            .map(|a| a.attachment_id.to_string())
            .collect(),
        incoming: matches!(payload.direction, peppy_client_core::Direction::Incoming),
        seen: false,
        send_state: None,
    }
}

fn transport_name(transport: Transport) -> &'static str {
    match transport {
        Transport::Sms => "sms",
        Transport::Mms => "mms",
        Transport::Rcs => "rcs",
    }
}

fn attachment_state(state: AttachmentState) -> NativeAttachmentState {
    match state {
        AttachmentState::PendingUpload => NativeAttachmentState::PendingUpload,
        AttachmentState::Uploaded => NativeAttachmentState::Uploaded,
        AttachmentState::PendingDownload => NativeAttachmentState::PendingDownload,
        AttachmentState::Available => NativeAttachmentState::Available,
    }
}

fn attachment_view(info: AttachmentInfo) -> NativeAttachmentInfo {
    NativeAttachmentInfo {
        attachment_id: info.attachment_id.to_string(),
        media_type: info.media_type,
        display_name: info.display_name,
        plaintext_bytes: info.plaintext_bytes,
        ciphertext_bytes: info.ciphertext_bytes,
        ciphertext_sha256: info.ciphertext_sha256,
        state: attachment_state(info.state),
    }
}

fn cipher_view(object: CipherObject) -> NativeCipherObject {
    NativeCipherObject {
        attachment_id: object.attachment_id.to_string(),
        ciphertext_bytes: object.ciphertext_bytes,
        ciphertext_sha256: object.ciphertext_sha256,
        remote_object_id: object.remote_object_id,
    }
}

fn send_result(result: NativeSendResult) -> SendResult {
    match result {
        NativeSendResult::Submitted => SendResult::Submitted,
        NativeSendResult::Sent => SendResult::Sent,
        NativeSendResult::Delivered => SendResult::Delivered,
        NativeSendResult::FailedBeforeSubmission => SendResult::FailedBeforeSubmission,
        NativeSendResult::FailedConfirmed => SendResult::FailedConfirmed,
    }
}

fn permit_view(decision: PermitDecision) -> NativePermit {
    match decision {
        PermitDecision::Permit(ReceivedCommand {
            command_id,
            subscription_id,
            message,
        }) => NativePermit {
            state: NativePermitState::Permit,
            command: Some(NativeCarrierCommand {
                command_id: command_id.to_string(),
                subscription_id,
                message: payload_view(message),
            }),
            existing_send_state: None,
        },
        PermitDecision::AlreadyAttempted(state) => NativePermit {
            state: NativePermitState::AlreadyAttempted,
            command: None,
            existing_send_state: Some(format!("{state:?}")),
        },
        PermitDecision::Blocked(block) => NativePermit {
            state: match block {
                PermitBlock::NotReceived => NativePermitState::NotReceived,
                PermitBlock::StaleEpoch => NativePermitState::StaleEpoch,
                PermitBlock::EpochNotActive => NativePermitState::EpochNotActive,
                PermitBlock::MediaUnavailable => NativePermitState::MediaUnavailable,
                PermitBlock::Historical => NativePermitState::Historical,
                PermitBlock::RestoreGuarded => NativePermitState::RestoreGuarded,
            },
            command: None,
            existing_send_state: None,
        },
    }
}

/// Admits one operation against the native handle. The lifetime mutex is held
/// only long enough to clone the shared core `Client` handle; `operation` then
/// runs with that mutex released, so slow work (Argon2 unlock, media
/// verification) never blocks other admissions such as receiver capture. The
/// core store remains the sole serializer of database access.
fn with_client<T>(
    handle: &Mutex<Option<Client>>,
    operation: impl FnOnce(&Client) -> Result<T, CoreError>,
) -> Result<T, MobileBindingsError> {
    let client = {
        let guard = handle.lock().map_err(|_| MobileBindingsError::Database)?;
        guard.as_ref().ok_or(MobileBindingsError::Closed)?.clone()
    };
    operation(&client).map_err(Into::into)
}

fn draft_view(
    draft: peppy_client_core::ComposeDraft,
) -> Result<NativeComposeDraft, MobileBindingsError> {
    Ok(NativeComposeDraft {
        draft_id: draft.draft_id.to_string(),
        conversation_id: draft.conversation_id.to_string(),
        text: draft.text,
        recipients: draft.recipients,
        attachment_ids: draft
            .attachment_ids
            .into_iter()
            .map(|id| id.to_string())
            .collect(),
        route_json: draft
            .route
            .map(|route| serde_json::to_string(&route))
            .transpose()
            .map_err(|_| MobileBindingsError::Database)?,
        revision: draft.revision,
    })
}

fn draft_update(
    update: NativeComposeDraftUpdate,
) -> Result<ComposeDraftUpdate, MobileBindingsError> {
    Ok(ComposeDraftUpdate {
        text: update.text,
        recipients: update.recipients,
        attachment_ids: update
            .attachment_ids
            .into_iter()
            .map(|id| parse_id(&id))
            .collect::<Result<_, _>>()?,
        route: update
            .route_json
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|_| MobileBindingsError::InvalidRequest)?,
    })
}

fn mms_input(input: NativeMmsAcquisitionInput) -> MmsAcquisitionInput {
    MmsAcquisitionInput {
        source: MmsSource {
            source_generation: input.source.source_generation,
            subscription_id: input.source.subscription_id,
            provider_message_id: input.source.provider_message_id,
            provider_thread_id: input.source.provider_thread_id,
        },
        direction: if input.incoming {
            peppy_client_core::Direction::Incoming
        } else {
            peppy_client_core::Direction::Outgoing
        },
        sender_address: input.sender_address,
        recipients: input.recipients,
        subject: input.subject,
        body: input.body,
        imported: input.imported,
        observed_at_ms: input.observed_at_ms,
        transaction_id: input.transaction_id,
    }
}
fn mms_input_view(input: MmsAcquisitionInput) -> NativeMmsAcquisitionInput {
    NativeMmsAcquisitionInput {
        source: NativeMmsSource {
            source_generation: input.source.source_generation,
            subscription_id: input.source.subscription_id,
            provider_message_id: input.source.provider_message_id,
            provider_thread_id: input.source.provider_thread_id,
        },
        incoming: input.direction == peppy_client_core::Direction::Incoming,
        sender_address: input.sender_address,
        recipients: input.recipients,
        subject: input.subject,
        body: input.body,
        imported: input.imported,
        observed_at_ms: input.observed_at_ms,
        transaction_id: input.transaction_id,
    }
}
fn mms_state(state: NativeMmsAcquisitionState) -> MmsAcquisitionState {
    match state {
        NativeMmsAcquisitionState::Pending => MmsAcquisitionState::Pending,
        NativeMmsAcquisitionState::Blocked => MmsAcquisitionState::Blocked,
        NativeMmsAcquisitionState::Unavailable => MmsAcquisitionState::Unavailable,
        NativeMmsAcquisitionState::Complete => MmsAcquisitionState::Complete,
    }
}
fn mms_state_view(state: MmsAcquisitionState) -> NativeMmsAcquisitionState {
    match state {
        MmsAcquisitionState::Pending => NativeMmsAcquisitionState::Pending,
        MmsAcquisitionState::Blocked => NativeMmsAcquisitionState::Blocked,
        MmsAcquisitionState::Unavailable => NativeMmsAcquisitionState::Unavailable,
        MmsAcquisitionState::Complete => NativeMmsAcquisitionState::Complete,
    }
}
fn mms_acquisition_view(item: peppy_client_core::MmsAcquisition) -> NativeMmsAcquisition {
    NativeMmsAcquisition {
        acquisition_id: item.acquisition_id,
        conversation_id: item.conversation_id.to_string(),
        input: mms_input_view(item.input),
        state: mms_state_view(item.state),
        reason: item.reason,
        attachment_ids: item
            .attachment_ids
            .into_iter()
            .map(|id| id.to_string())
            .collect(),
    }
}

#[uniffi::export]
pub fn open_native_client(
    config: NativeOpenConfig,
) -> Result<Arc<NativeClient>, MobileBindingsError> {
    let vault_id = VaultId(
        uuid::Uuid::parse_str(&config.vault_id).map_err(|_| MobileBindingsError::InvalidRequest)?,
    );
    let device_id = DeviceId(
        uuid::Uuid::parse_str(&config.device_id)
            .map_err(|_| MobileBindingsError::InvalidRequest)?,
    );
    let mut database_key = config.database_key;
    let key = DatabaseKey::new(&database_key)?;
    database_key.zeroize();
    let client = Client::open(
        ClientConfig {
            database_path: config.database_path.into(),
            vault_id,
            device_id,
        },
        key,
    )?;
    Ok(Arc::new(NativeClient {
        client: Mutex::new(Some(client)),
    }))
}

/// Creates synthetic test enrollment material for host smoke tests only. Production
/// enrollment supplies its already-authenticated profile/header through `unlock`.
#[uniffi::export]
pub fn create_smoke_vault_material(
    vault_id: String,
    passphrase: String,
) -> Result<NativeVaultMaterial, MobileBindingsError> {
    let vault =
        uuid::Uuid::parse_str(&vault_id).map_err(|_| MobileBindingsError::InvalidRequest)?;
    let profile = KeyProfile::new(vault, 1).map_err(|_| MobileBindingsError::Crypto)?;
    let root = peppy_crypto::derive_root_key(&passphrase, &profile)
        .map_err(|_| MobileBindingsError::Crypto)?;
    let header = peppy_crypto::create_vault_check_header(&root, profile.clone())
        .map_err(|_| MobileBindingsError::Crypto)?;
    Ok(NativeVaultMaterial {
        profile_json: serde_json::to_string(&profile).map_err(|_| MobileBindingsError::Database)?,
        header_json: serde_json::to_string(&header).map_err(|_| MobileBindingsError::Database)?,
    })
}

#[uniffi::export]
pub fn android_companion_health() -> GatewayHealth {
    GatewayHealth { enrollment_state: "Not enrolled — diagnostic shell only".into(), diagnostics: vec![
        CapabilityDiagnostic { name: "SMS companion".into(), state: CapabilityState::PermissionRequired, detail: "Runtime SMS permissions are required; this app does not request the default SMS role.".into() },
        CapabilityDiagnostic { name: "RCS gateway".into(), state: CapabilityState::Unsupported, detail: "Ordinary Android apps have no general RCS inbox or send API.".into() },
    ] }
}

#[uniffi::export]
impl NativeClient {
    /// Closes this handle to new operations: every later call returns `Closed`.
    /// Operations admitted before `dispose` keep their own core handle and run
    /// to completion; the underlying store closes once the last one finishes.
    pub fn dispose(&self) -> Result<(), MobileBindingsError> {
        let mut guard = self
            .client
            .lock()
            .map_err(|_| MobileBindingsError::Database)?;
        if guard.take().is_none() {
            return Err(MobileBindingsError::Closed);
        }
        Ok(())
    }

    pub fn unlock(
        &self,
        profile_json: String,
        header_json: String,
        passphrase: String,
    ) -> Result<(), MobileBindingsError> {
        let profile: KeyProfile =
            serde_json::from_str(&profile_json).map_err(|_| MobileBindingsError::InvalidProfile)?;
        let header: VaultCheckHeader =
            serde_json::from_str(&header_json).map_err(|_| MobileBindingsError::InvalidProfile)?;
        with_client(&self.client, |client| {
            client.unlock(&profile, &header, &passphrase)
        })
    }

    /// Explicit manual epoch cutover. Native hosts call this only after a
    /// successful passphrase/header `unlock` of the newer profile; never after
    /// credential import or native-key-cache restoration. Repeating the active
    /// epoch is an idempotent no-op. Higher epochs remain subject to C's
    /// verified-and-unlocked requirement.
    pub fn activate_verified_epoch(&self, epoch: u32) -> Result<(), MobileBindingsError> {
        with_client(&self.client, |client| {
            if client.key_status()?.active_epoch == Some(epoch) {
                Ok(())
            } else {
                client.activate_epoch(epoch)
            }
        })
    }

    pub fn capture_incoming(
        &self,
        sms: NativeIncomingSms,
    ) -> Result<NativeCaptured, MobileBindingsError> {
        let conversation_id = sms.conversation_id.as_deref().map(parse_id).transpose()?;
        with_client(&self.client, |client| {
            client.capture_incoming(IncomingSms {
                conversation_id,
                sender_address: sms.sender_address,
                body: sms.body,
                provider_message_id: sms.provider_message_id,
                imported: sms.imported,
            })
        })
        .map(
            |Captured {
                 message_id,
                 conversation_id,
                 duplicate,
             }| NativeCaptured {
                message_id: message_id.to_string(),
                conversation_id: conversation_id.to_string(),
                duplicate,
            },
        )
    }

    pub fn begin_mms_acquisition(
        &self,
        input: NativeMmsAcquisitionInput,
    ) -> Result<NativeMmsAcquisition, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.begin_mms_acquisition(mms_input(input))
        })
        .map(mms_acquisition_view)
    }
    pub fn mms_acquisitions(
        &self,
        limit: u64,
    ) -> Result<Vec<NativeMmsAcquisition>, MobileBindingsError> {
        let limit = usize::try_from(limit).map_err(|_| MobileBindingsError::InvalidRequest)?;
        with_client(&self.client, |client| client.mms_acquisitions(limit))
            .map(|items| items.into_iter().map(mms_acquisition_view).collect())
    }
    pub fn set_mms_acquisition_state(
        &self,
        id: String,
        state: NativeMmsAcquisitionState,
        reason: Option<String>,
    ) -> Result<(), MobileBindingsError> {
        with_client(&self.client, |client| {
            client.set_mms_acquisition_state(&id, mms_state(state), reason.as_deref())
        })
    }
    pub fn set_mms_acquisition_part(
        &self,
        id: String,
        provider_part_id: String,
        attachment_id: String,
    ) -> Result<(), MobileBindingsError> {
        let attachment_id = parse_id(&attachment_id)?;
        with_client(&self.client, |client| {
            client.set_mms_acquisition_part(&id, &provider_part_id, attachment_id)
        })
    }
    pub fn mms_acquisition_parts(
        &self,
        id: String,
    ) -> Result<Vec<NativeMmsAcquisitionPart>, MobileBindingsError> {
        with_client(&self.client, |client| client.mms_acquisition_parts(&id)).map(|parts| {
            parts
                .into_iter()
                .map(|part| NativeMmsAcquisitionPart {
                    provider_part_id: part.provider_part_id,
                    attachment_id: part.attachment_id.to_string(),
                })
                .collect()
        })
    }
    pub fn complete_mms_acquisition(
        &self,
        id: String,
    ) -> Result<NativeCaptured, MobileBindingsError> {
        with_client(&self.client, |client| client.complete_mms_acquisition(&id)).map(|captured| {
            NativeCaptured {
                message_id: captured.message_id.to_string(),
                conversation_id: captured.conversation_id.to_string(),
                duplicate: captured.duplicate,
            }
        })
    }
    pub fn mms_scan_checkpoint(
        &self,
        source_generation: String,
        subscription_id: String,
        imported: bool,
    ) -> Result<Option<String>, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.mms_scan_checkpoint(&source_generation, &subscription_id, imported)
        })
    }
    pub fn set_mms_scan_checkpoint(
        &self,
        source_generation: String,
        subscription_id: String,
        imported: bool,
        provider_message_id: String,
    ) -> Result<(), MobileBindingsError> {
        with_client(&self.client, |client| {
            client.set_mms_scan_checkpoint(
                &source_generation,
                &subscription_id,
                imported,
                &provider_message_id,
            )
        })
    }
    pub fn mms_pending_media_bytes(&self) -> Result<u64, MobileBindingsError> {
        with_client(&self.client, Client::mms_pending_media_bytes)
    }
    pub fn set_mms_own_address(
        &self,
        subscription_id: String,
        address: String,
    ) -> Result<(), MobileBindingsError> {
        with_client(&self.client, |client| {
            client.set_mms_own_address(&subscription_id, &address)
        })
    }
    pub fn mms_reply_context(
        &self,
        conversation_id: String,
    ) -> Result<NativeMmsReplyContext, MobileBindingsError> {
        let conversation_id = parse_id(&conversation_id)?;
        with_client(&self.client, |client| {
            client.mms_reply_context(conversation_id)
        })
        .map(|context| NativeMmsReplyContext {
            recipients: context.recipients,
            blocked_reason: context.blocked_reason,
            subject: context.subject,
        })
    }

    pub fn notification_source_device_id(&self) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            // The core config identity is intentionally not otherwise exposed; this is a stable
            // non-secret source identity needed by the Android listener.
            client.notification_source_device_id()
        })
    }

    /// Contact DTOs are Rust-owned JSON schemas; native hosts must not maintain a parallel wire model.
    pub fn capture_contact_book(&self, input_json: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.capture_contact_book(&input_json)
        })
    }
    pub fn capture_platform_contacts_json(
        &self,
        input_json: String,
    ) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.capture_platform_contacts_json(&input_json)
        })
    }
    pub fn contact_scan_state_json(
        &self,
        input_json: String,
    ) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.contact_scan_state_json(&input_json)
        })
    }
    pub fn contact_apply_evidence_json(
        &self,
        input_json: String,
    ) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.contact_apply_evidence_json(&input_json)
        })
    }
    pub fn contact_source_context_json(
        &self,
        input_json: String,
    ) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.contact_source_context_json(&input_json)
        })
    }
    pub fn begin_contact_scan(&self, input_json: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.begin_contact_scan(&input_json)
        })
    }
    pub fn observe_contact_scan(&self, input_json: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.observe_contact_scan(&input_json)
        })
    }
    pub fn finish_contact_scan(&self, input_json: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.finish_contact_scan(&input_json)
        })
    }
    pub fn contact_book_view(&self, input_json: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| client.contact_book_view(&input_json))
    }
    pub fn request_contact_edit(&self, input_json: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.request_contact_edit(&input_json)
        })
    }
    pub fn next_contact_apply_permit(
        &self,
        input_json: String,
    ) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.next_contact_apply_permit(&input_json)
        })
    }
    pub fn reconcile_contact_apply(
        &self,
        input_json: String,
    ) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.reconcile_contact_apply(&input_json)
        })
    }
    pub fn forget_contact_book(&self, input_json: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.forget_contact_book(&input_json)
        })
    }

    // Contact query methods (Lane B4)
    pub fn list_contact_books_json(&self) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| client.list_contact_books_json())
    }

    pub fn contact_settings_json(&self, input: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| client.contact_settings_json(&input))
    }

    pub fn list_contact_requests_json(&self, input: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.list_contact_requests_json(&input)
        })
    }

    pub fn contact_approval_json(&self, input: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| client.contact_approval_json(&input))
    }

    pub fn list_restorable_contacts_json(
        &self,
        input: String,
    ) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.list_restorable_contacts_json(&input)
        })
    }

    pub fn restore_contact_json(&self, input: String) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| client.restore_contact_json(&input))
    }

    pub fn prepare_contact_photo(
        &self,
        path: String,
    ) -> Result<NativeAttachmentInfo, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.prepare_contact_photo(std::path::Path::new(&path))
        })
        .map(attachment_view)
    }

    /// Contact photo work queue JSON (uploads with `reference_tracking`, reference
    /// registrations to POST before publishing, reclaim candidates).
    pub fn contact_photo_transfer_state_json(&self) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.contact_photo_transfer_state_json()
        })
    }

    pub fn acknowledge_contact_photo_reference_json(
        &self,
        input_json: String,
    ) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.acknowledge_contact_photo_reference_json(&input_json)
        })
    }

    pub fn acknowledge_contact_photo_reclaim_json(
        &self,
        input_json: String,
    ) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.acknowledge_contact_photo_reclaim_json(&input_json)
        })
    }

    pub fn capture_notification(
        &self,
        input: NativeNotificationCapture,
    ) -> Result<NativeNotificationCaptureOutcome, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.capture_notification(NotificationCapture {
                notification_key: input.notification_key,
                instance: input.instance,
                package_name: input.package_name,
                app_name: input.app_name,
                title: input.title,
                text: input.text,
                category: input.category,
                posted_at: input.posted_at,
                dismissible: input.dismissible,
            })
        })
        .map(|outcome| match outcome {
            NotificationCaptureOutcome::Captured => NativeNotificationCaptureOutcome::Captured,
            NotificationCaptureOutcome::Duplicate => NativeNotificationCaptureOutcome::Duplicate,
            NotificationCaptureOutcome::FilteredOut => {
                NativeNotificationCaptureOutcome::FilteredOut
            }
            NotificationCaptureOutcome::DroppedLocked => {
                NativeNotificationCaptureOutcome::DroppedLocked
            }
        })
    }
    pub fn remove_notification(
        &self,
        notification_key: String,
        instance: String,
    ) -> Result<(), MobileBindingsError> {
        with_client(&self.client, |client| {
            client.remove_notification(&notification_key, &instance)
        })
    }
    pub fn notification_snapshot(&self) -> Result<NativeNotificationSnapshot, MobileBindingsError> {
        with_client(&self.client, Client::notification_snapshot).map(notification_snapshot_view)
    }
    pub fn set_app_muted(
        &self,
        source_device_id: String,
        package_name: String,
        app_name: String,
        muted: bool,
    ) -> Result<(), MobileBindingsError> {
        with_client(&self.client, |c| {
            c.set_app_muted(&source_device_id, &package_name, &app_name, muted)
        })
    }
    pub fn dismiss_notification(
        &self,
        target: NativeNotificationTarget,
    ) -> Result<(), MobileBindingsError> {
        with_client(&self.client, |c| {
            c.dismiss_notification(notification_target(target))
        })
    }
    pub fn mark_notifications_seen(
        &self,
        targets: Vec<NativeNotificationTarget>,
    ) -> Result<(), MobileBindingsError> {
        with_client(&self.client, |c| {
            c.mark_notifications_seen(targets.into_iter().map(notification_target).collect())
        })
    }
    pub fn pending_notification_dismissals(
        &self,
        limit: u64,
    ) -> Result<Vec<NativeNotificationDismissal>, MobileBindingsError> {
        let limit = usize::try_from(limit).map_err(|_| MobileBindingsError::InvalidRequest)?;
        with_client(&self.client, |c| c.pending_notification_dismissals(limit)).map(|items| {
            items
                .into_iter()
                .map(|d| NativeNotificationDismissal {
                    id: d.id,
                    target: notification_target_view(d.target),
                    instance: d.instance,
                })
                .collect()
        })
    }
    pub fn complete_notification_dismissal(&self, id: String) -> Result<(), MobileBindingsError> {
        with_client(&self.client, |c| c.complete_notification_dismissal(&id))
    }

    pub fn list_conversations(&self) -> Result<Vec<NativeConversation>, MobileBindingsError> {
        with_client(&self.client, Client::list_conversations).map(|items| {
            items
                .into_iter()
                .map(|item| NativeConversation {
                    conversation_id: item.conversation_id.to_string(),
                    unread_count: item.unread_count,
                })
                .collect()
        })
    }

    pub fn messages(
        &self,
        conversation_id: String,
    ) -> Result<Vec<NativeMessage>, MobileBindingsError> {
        let conversation_id = parse_id(&conversation_id)?;
        with_client(&self.client, |client| client.messages(conversation_id))
            .map(|items| items.into_iter().map(message_view).collect())
    }

    pub fn mark_seen(&self, message_id: String) -> Result<bool, MobileBindingsError> {
        let message_id = parse_id(&message_id)?;
        with_client(&self.client, |client| client.mark_seen(message_id))
    }

    pub fn pending_outbox_json(&self) -> Result<Vec<String>, MobileBindingsError> {
        with_client(&self.client, Client::pending_outbox).and_then(|items| {
            items
                .into_iter()
                .map(|item| serde_json::to_string(&item).map_err(|_| MobileBindingsError::Database))
                .collect()
        })
    }

    /// Bounded opaque outbox retrieval for native upload workers, capped at 500.
    ///
    /// Each non-zero call first makes bounded sealing progress: if the active
    /// epoch is unlocked, the core seals at most `min(limit, MAX_SEAL_BATCH)`
    /// of the oldest unsealed rows (captures beyond what unlock/import sealed
    /// implicitly). While locked, sealing is a no-op and already-sealed rows
    /// are still returned. No epoch is activated and nothing touches the
    /// network. It then returns at most `limit` of the oldest sealed,
    /// unacknowledged envelopes, preserving canonical ordering and
    /// byte-identical retries. Repeated calls (acking between them) therefore
    /// eventually expose every sealable row. `limit == 0` returns empty and
    /// seals nothing.
    pub fn pending_outbox_json_batch(
        &self,
        limit: u64,
    ) -> Result<Vec<String>, MobileBindingsError> {
        let limit =
            usize::try_from(limit.min(500)).map_err(|_| MobileBindingsError::InvalidRequest)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        with_client(&self.client, |client| {
            client.seal_pending_batch(limit.min(MAX_SEAL_BATCH))?;
            client.pending_outbox_batch(limit)
        })
        .and_then(|items| {
            items
                .into_iter()
                .map(|item| serde_json::to_string(&item).map_err(|_| MobileBindingsError::Database))
                .collect()
        })
    }

    pub fn ack_outbox(&self, envelope_id: String) -> Result<(), MobileBindingsError> {
        let envelope_id = parse_id(&envelope_id)?;
        with_client(&self.client, |client| client.ack_outbox(envelope_id))
    }

    /// Journals opaque server envelope JSON. Native code owns the transport and cursor progression.
    pub fn ingest_raw(
        &self,
        envelope_json: Vec<u8>,
        cursor: String,
    ) -> Result<NativeIngestResult, MobileBindingsError> {
        let cursor = parse_cursor(cursor)?;
        with_client(&self.client, |client| {
            client.ingest_raw(&envelope_json, cursor)
        })
        .map(|result| match result {
            peppy_client_core::IngestResult::Journaled => NativeIngestResult {
                state: NativeIngestState::Journaled,
                quarantine_reason: None,
            },
            peppy_client_core::IngestResult::Duplicate => NativeIngestResult {
                state: NativeIngestState::Duplicate,
                quarantine_reason: None,
            },
            peppy_client_core::IngestResult::Quarantined(reason) => NativeIngestResult {
                state: NativeIngestState::Quarantined,
                quarantine_reason: Some(format!("{reason:?}")),
            },
        })
    }

    pub fn receive_cursor(&self) -> Result<String, MobileBindingsError> {
        with_client(&self.client, Client::receive_cursor).map(|cursor| cursor.0.to_string())
    }

    pub fn apply_pending(&self, limit: u64) -> Result<NativeApplyReport, MobileBindingsError> {
        let limit = usize::try_from(limit).map_err(|_| MobileBindingsError::InvalidRequest)?;
        with_client(&self.client, |client| client.apply_pending(limit)).map(|report| {
            NativeApplyReport {
                applied: report.applied as u64,
                quarantined: report.quarantined as u64,
                waiting_for_keys: report.waiting_for_keys as u64,
                drained: report.drained as u64,
                snapshot_remaining: report.snapshot_remaining,
                superseded: report.superseded as u64,
            }
        })
    }

    pub fn begin_snapshot(
        &self,
        high_water: String,
        record_count: u64,
        purpose: NativeSnapshotPurpose,
    ) -> Result<NativeSnapshotProgress, MobileBindingsError> {
        let high_water = parse_cursor(high_water)?;
        let purpose = match purpose {
            NativeSnapshotPurpose::Resync => peppy_client_core::SnapshotPurpose::Resync,
            NativeSnapshotPurpose::Restore => peppy_client_core::SnapshotPurpose::Restore,
        };
        with_client(&self.client, |client| {
            client.begin_snapshot(high_water, record_count, purpose)
        })
        .map(snapshot_progress_view)
    }

    pub fn begin_snapshot_with_compaction(
        &self,
        high_water: String,
        record_count: u64,
        purpose: NativeSnapshotPurpose,
        server_compaction_generation: Option<String>,
    ) -> Result<NativeSnapshotProgress, MobileBindingsError> {
        let high_water = parse_cursor(high_water)?;
        let server_compaction_generation = server_compaction_generation
            .map(|value| parse_canonical_u64(&value))
            .transpose()?;
        let purpose = match purpose {
            NativeSnapshotPurpose::Resync => peppy_client_core::SnapshotPurpose::Resync,
            NativeSnapshotPurpose::Restore => peppy_client_core::SnapshotPurpose::Restore,
        };
        with_client(&self.client, |client| {
            client.begin_snapshot_with_compaction(
                high_water,
                record_count,
                purpose,
                server_compaction_generation,
            )
        })
        .map(snapshot_progress_view)
    }

    pub fn set_server_compaction_supported(
        &self,
        supported: bool,
    ) -> Result<(), MobileBindingsError> {
        with_client(&self.client, |client| {
            client.set_server_compaction_supported(supported)
        })
    }

    /// Records `/v1/snapshot` `compaction_supported` and `compaction_active`, runs one bounded
    /// frontier backfill step and returns the contact sync readiness JSON.
    pub fn set_server_compaction_state(
        &self,
        supported: bool,
        active: bool,
    ) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.set_server_compaction_state(supported, active)
        })
    }

    /// Contact sync readiness JSON (`state`: server_unsupported | needs_unlock |
    /// backfill_pending | ready). Contact producers fail closed unless `ready`.
    pub fn contact_sync_readiness_json(&self) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| client.contact_sync_readiness_json())
    }

    /// One bounded compaction frontier backfill step (readiness JSON plus `processed`).
    pub fn compaction_backfill_step_json(&self, limit: u32) -> Result<String, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.compaction_backfill_step_json(limit)
        })
    }

    pub fn server_compaction_supported(&self) -> Result<bool, MobileBindingsError> {
        with_client(&self.client, Client::server_compaction_supported)
    }

    /// True after a schema upgrade or `request_contact_repair` until a compaction snapshot
    /// promotes. While true, hosts fetch one fenced snapshot (`begin_snapshot_with_compaction`)
    /// instead of republishing owned contacts.
    pub fn contact_repair_required(&self) -> Result<bool, MobileBindingsError> {
        with_client(&self.client, Client::contact_repair_required)
    }

    /// Latches a local projection repair (e.g. after an integrity warning). Does not clear
    /// visible state; the next promoted compaction snapshot replaces it.
    pub fn request_contact_repair(&self) -> Result<(), MobileBindingsError> {
        with_client(&self.client, Client::request_contact_repair)
    }

    pub fn snapshot_projection_status(
        &self,
    ) -> Result<Option<NativeSnapshotProjectionStatus>, MobileBindingsError> {
        use peppy_client_core::SnapshotProjectionState as S;
        with_client(&self.client, Client::snapshot_projection_status).map(|status| {
            status.map(|status| NativeSnapshotProjectionStatus {
                generation: status.generation,
                high_water: status.high_water.0.to_string(),
                state: match status.state {
                    S::Draining => NativeSnapshotProjectionState::Draining,
                    S::Staging => NativeSnapshotProjectionState::Staging,
                    S::Promoted => NativeSnapshotProjectionState::Promoted,
                    S::Failed => NativeSnapshotProjectionState::Failed,
                },
                reason: status.reason,
            })
        })
    }

    pub fn snapshot_progress(&self) -> Result<Option<NativeSnapshotProgress>, MobileBindingsError> {
        with_client(&self.client, Client::snapshot_progress)
            .map(|progress| progress.map(snapshot_progress_view))
    }

    pub fn append_snapshot_raw_page(
        &self,
        generation: u64,
        records: Vec<NativeRawSnapshotRecord>,
    ) -> Result<NativeSnapshotProgress, MobileBindingsError> {
        let records = records
            .into_iter()
            .map(|record| {
                Ok(peppy_client_core::RawSnapshotRecord {
                    cursor: parse_cursor(record.cursor)?,
                    envelope_json: record.envelope_json,
                })
            })
            .collect::<Result<Vec<_>, MobileBindingsError>>()?;
        with_client(&self.client, |client| {
            client.append_snapshot_raw_page(generation, &records)
        })
        .map(snapshot_progress_view)
    }

    /// Publishes only. Host work loops must drain with `apply_pending` until
    /// `NativeApplyReport.snapshot_remaining` is zero.
    pub fn finish_snapshot(
        &self,
        generation: u64,
    ) -> Result<NativeSnapshotReport, MobileBindingsError> {
        with_client(&self.client, |client| client.finish_snapshot(generation)).map(|report| {
            NativeSnapshotReport {
                journaled: report.journaled as u64,
                duplicate: report.duplicate as u64,
                quarantined: report.quarantined as u64,
                receive_cursor: report.receive_cursor.to_string(),
            }
        })
    }

    pub fn pending_commands(&self) -> Result<Vec<NativeCarrierCommand>, MobileBindingsError> {
        with_client(&self.client, Client::pending_commands).map(|commands| {
            commands
                .into_iter()
                .map(
                    |ReceivedCommand {
                         command_id,
                         subscription_id,
                         message,
                     }| NativeCarrierCommand {
                        command_id: command_id.to_string(),
                        subscription_id,
                        message: payload_view(message),
                    },
                )
                .collect()
        })
    }

    /// Lets an SMS-only adapter reject an immutable pending MMS command before
    /// requesting a carrier permit. It exposes no attachment metadata or keys.
    pub fn pending_command_attachment_count(
        &self,
        command_id: String,
    ) -> Result<u64, MobileBindingsError> {
        let command_id = parse_id(&command_id)?;
        with_client(&self.client, Client::pending_commands).and_then(|commands| {
            commands
                .into_iter()
                .find(|command| command.command_id == command_id)
                .map(|command| {
                    u64::try_from(command.message.record.attachments.len())
                        .map_err(|_| MobileBindingsError::Database)
                })
                .unwrap_or(Err(MobileBindingsError::NotFound))
        })
    }

    /// A carrier API may be invoked only when this returns NativePermitState::Permit.
    pub fn begin_send_attempt(
        &self,
        command_id: String,
    ) -> Result<NativePermit, MobileBindingsError> {
        let command_id = parse_id(&command_id)?;
        with_client(&self.client, |client| client.begin_send_attempt(command_id)).map(permit_view)
    }

    pub fn record_send_result(
        &self,
        command_id: String,
        result: NativeSendResult,
    ) -> Result<String, MobileBindingsError> {
        let command_id = parse_id(&command_id)?;
        with_client(&self.client, |client| {
            client.record_send_result(command_id, send_result(result))
        })
        .map(|state| format!("{state:?}"))
    }

    /// Sensitive bytes for Android Keystore/iOS Keychain/native secure storage only.
    /// This API is deliberately not represented in a view record.
    pub fn export_native_key_cache_for_native_storage(
        &self,
        epoch: u32,
    ) -> Result<Vec<u8>, MobileBindingsError> {
        with_client(&self.client, |client| client.export_native_key_cache(epoch))
            .map(|cache| cache.native_storage_bytes().to_vec())
    }

    /// Imports the opaque bytes previously retrieved from native secure storage.
    pub fn import_native_key_cache_from_native_storage(
        &self,
        bytes: Vec<u8>,
    ) -> Result<(), MobileBindingsError> {
        let cache = NativeKeyCache::from_native_storage(bytes);
        with_client(&self.client, |client| {
            client.import_native_key_cache(&cache)
        })
    }

    pub fn create_compose_draft(
        &self,
        conversation_id: Option<String>,
    ) -> Result<NativeComposeDraft, MobileBindingsError> {
        let conversation_id = conversation_id.as_deref().map(parse_id).transpose()?;
        with_client(&self.client, |client| {
            client.create_compose_draft(conversation_id)
        })
        .and_then(draft_view)
    }

    pub fn compose_drafts(&self) -> Result<Vec<NativeComposeDraft>, MobileBindingsError> {
        with_client(&self.client, Client::compose_drafts)
            .and_then(|drafts| drafts.into_iter().map(draft_view).collect())
    }

    pub fn save_compose_draft(
        &self,
        draft_id: String,
        expected_revision: u64,
        update: NativeComposeDraftUpdate,
    ) -> Result<NativeComposeDraft, MobileBindingsError> {
        let draft_id = parse_id(&draft_id)?;
        let update = draft_update(update)?;
        with_client(&self.client, |client| {
            client.save_compose_draft(draft_id, expected_revision, update)
        })
        .and_then(draft_view)
    }

    pub fn send_compose_draft(
        &self,
        draft_id: String,
        expected_revision: u64,
    ) -> Result<NativeQueuedSend, MobileBindingsError> {
        let draft_id = parse_id(&draft_id)?;
        with_client(&self.client, |client| {
            client.send_compose_draft(draft_id, expected_revision)
        })
        .map(|queued| NativeQueuedSend {
            message_id: queued.message_id.to_string(),
            command_id: queued.command_id.to_string(),
            envelope_id: queued.envelope_id.to_string(),
        })
    }

    pub fn send_compose_draft_checked_transport(
        &self,
        draft_id: String,
        expected_revision: u64,
        expected_transport: String,
    ) -> Result<NativeQueuedSend, MobileBindingsError> {
        let draft_id = parse_id(&draft_id)?;
        let expected_transport = match expected_transport.as_str() {
            "sms" => Transport::Sms,
            "mms" => Transport::Mms,
            "rcs" => Transport::Rcs,
            _ => return Err(MobileBindingsError::InvalidRequest),
        };
        with_client(&self.client, |client| {
            client.send_compose_draft_checked_transport(
                draft_id,
                expected_revision,
                expected_transport,
            )
        })
        .map(|queued| NativeQueuedSend {
            message_id: queued.message_id.to_string(),
            command_id: queued.command_id.to_string(),
            envelope_id: queued.envelope_id.to_string(),
        })
    }

    pub fn prepare_attachment(
        &self,
        source_path: String,
        media_type: String,
        display_name: String,
    ) -> Result<NativeAttachmentInfo, MobileBindingsError> {
        with_client(&self.client, |client| {
            client.prepare_attachment(
                std::path::Path::new(&source_path),
                &media_type,
                &display_name,
            )
        })
        .map(attachment_view)
    }

    pub fn attachment_info(
        &self,
        attachment_id: String,
    ) -> Result<NativeAttachmentInfo, MobileBindingsError> {
        let attachment_id = parse_id(&attachment_id)?;
        with_client(&self.client, |client| client.attachment_info(attachment_id))
            .map(attachment_view)
    }
    pub fn discard_unreferenced_attachment(
        &self,
        attachment_id: String,
    ) -> Result<bool, MobileBindingsError> {
        let attachment_id = parse_id(&attachment_id)?;
        with_client(&self.client, |client| {
            client.discard_unreferenced_attachment(attachment_id)
        })
    }

    pub fn pending_uploads(&self) -> Result<Vec<NativeCipherObject>, MobileBindingsError> {
        with_client(&self.client, Client::pending_uploads)
            .map(|items| items.into_iter().map(cipher_view).collect())
    }

    pub fn pending_downloads(&self) -> Result<Vec<NativeCipherObject>, MobileBindingsError> {
        with_client(&self.client, Client::pending_downloads)
            .map(|items| items.into_iter().map(cipher_view).collect())
    }

    /// Native-only verified cipher path for upload; never expose it to web content.
    pub fn native_cipher_file_for_upload(
        &self,
        attachment_id: String,
    ) -> Result<String, MobileBindingsError> {
        let attachment_id = parse_id(&attachment_id)?;
        with_client(&self.client, |client| {
            client.native_cipher_file(attachment_id)
        })
        .map(|path| path.to_string_lossy().into_owned())
    }

    pub fn mark_attachment_uploaded(
        &self,
        attachment_id: String,
        remote_object_id: String,
    ) -> Result<(), MobileBindingsError> {
        let attachment_id = parse_id(&attachment_id)?;
        with_client(&self.client, |client| {
            client.mark_attachment_uploaded(attachment_id, &remote_object_id)
        })
    }

    pub fn install_downloaded_attachment(
        &self,
        attachment_id: String,
        downloaded_path: String,
    ) -> Result<(), MobileBindingsError> {
        let attachment_id = parse_id(&attachment_id)?;
        with_client(&self.client, |client| {
            client.install_downloaded_attachment(
                attachment_id,
                std::path::Path::new(&downloaded_path),
            )
        })
    }

    pub fn open_native_plaintext_file(
        &self,
        attachment_id: String,
    ) -> Result<Arc<NativePlaintextHandle>, MobileBindingsError> {
        let attachment_id = parse_id(&attachment_id)?;
        with_client(&self.client, |client| {
            client.open_native_plaintext(attachment_id)
        })
        .map(|file| {
            Arc::new(NativePlaintextHandle {
                file: Mutex::new(Some(file)),
            })
        })
    }
}

#[uniffi::export]
impl NativePlaintextHandle {
    /// Native-only temporary plaintext path. Deleted on dispose/drop, not secure-erased.
    pub fn native_plaintext_path(&self) -> Result<String, MobileBindingsError> {
        let file = self.file.lock().map_err(|_| MobileBindingsError::Storage)?;
        file.as_ref()
            .map(|file| file.path().to_string_lossy().into_owned())
            .ok_or(MobileBindingsError::Closed)
    }
    pub fn dispose(&self) -> Result<(), MobileBindingsError> {
        let mut file = self.file.lock().map_err(|_| MobileBindingsError::Storage)?;
        if file.take().is_none() {
            return Err(MobileBindingsError::Closed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_transport_strings_match_carrier_dispatch_contract() {
        assert_eq!(transport_name(Transport::Sms), "sms");
        assert_eq!(transport_name(Transport::Mms), "mms");
        assert_eq!(transport_name(Transport::Rcs), "rcs");
    }
    #[test]
    fn contact_repair_and_projection_status_forward_to_core() {
        let client = open_native_client(NativeOpenConfig {
            database_path: std::env::temp_dir()
                .join(format!("peppy-mobile-repair-{}.db", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned(),
            vault_id: uuid::Uuid::new_v4().to_string(),
            device_id: uuid::Uuid::new_v4().to_string(),
            database_key: vec![9; 32],
        })
        .unwrap();
        assert!(!client.contact_repair_required().unwrap());
        client.request_contact_repair().unwrap();
        assert!(client.contact_repair_required().unwrap());
        assert_eq!(client.snapshot_projection_status().unwrap(), None);
        // A zero-record compaction snapshot promotes and clears the latch.
        let progress = client
            .begin_snapshot_with_compaction(
                "0".into(),
                0,
                NativeSnapshotPurpose::Resync,
                Some("3".into()),
            )
            .unwrap();
        client.finish_snapshot(progress.generation).unwrap();
        client.apply_pending(100).unwrap();
        let status = client.snapshot_projection_status().unwrap().unwrap();
        assert_eq!(status.state, NativeSnapshotProjectionState::Promoted);
        assert_eq!(status.reason, None);
        assert!(!client.contact_repair_required().unwrap());
    }

    #[test]
    fn closed_handle_is_typed() {
        let client = open_native_client(NativeOpenConfig {
            database_path: format!("/tmp/peppy-mobile-{}.db", uuid::Uuid::new_v4()),
            vault_id: uuid::Uuid::new_v4().to_string(),
            device_id: uuid::Uuid::new_v4().to_string(),
            database_key: vec![7; 32],
        })
        .unwrap();
        client.dispose().unwrap();
        assert!(matches!(
            client.list_conversations(),
            Err(MobileBindingsError::Closed)
        ));
    }

    #[test]
    fn admitted_operation_does_not_block_capture_and_survives_dispose() {
        use std::sync::mpsc;
        use std::time::Duration;

        let vault_id = uuid::Uuid::new_v4().to_string();
        let material =
            create_smoke_vault_material(vault_id.clone(), "test passphrase".into()).unwrap();
        let path =
            std::env::temp_dir().join(format!("peppy-mobile-lifetime-{}.db", uuid::Uuid::new_v4()));
        let native = open_native_client(NativeOpenConfig {
            database_path: path.to_string_lossy().into_owned(),
            vault_id,
            device_id: uuid::Uuid::new_v4().to_string(),
            database_key: vec![5; 32],
        })
        .unwrap();
        native
            .unlock(
                material.profile_json,
                material.header_json,
                "test passphrase".into(),
            )
            .unwrap();

        // The paused operation blocks on `release_rx`. Dropping `release_tx`
        // (including during a failed-assertion unwind) also releases it, so a
        // regression fails via the bounded wait below instead of deadlocking.
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let paused = {
            let native = Arc::clone(&native);
            std::thread::spawn(move || {
                with_client(&native.client, |client| {
                    entered_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                    client.list_conversations()
                })
            })
        };
        entered_rx.recv().expect("operation was admitted");

        let (done_tx, done_rx) = mpsc::channel();
        let concurrent = {
            let native = Arc::clone(&native);
            std::thread::spawn(move || {
                let captured = native.capture_incoming(NativeIncomingSms {
                    conversation_id: None,
                    sender_address: "+15550001111".into(),
                    body: "captured while another operation is admitted".into(),
                    provider_message_id: Some("lifetime-capture-1".into()),
                    imported: false,
                });
                let cursor = native.receive_cursor();
                let _ = done_tx.send((captured, cursor));
            })
        };
        let (captured, cursor) = done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("capture/status must not wait for an admitted operation");
        concurrent.join().unwrap();
        let captured = captured.unwrap();
        assert!(!captured.duplicate);
        assert_eq!(cursor.unwrap(), "0");

        native.dispose().unwrap();
        assert!(matches!(
            native.receive_cursor(),
            Err(MobileBindingsError::Closed)
        ));
        assert!(matches!(
            native.capture_incoming(NativeIncomingSms {
                conversation_id: None,
                sender_address: "+15550001111".into(),
                body: "after dispose".into(),
                provider_message_id: Some("lifetime-capture-2".into()),
                imported: false,
            }),
            Err(MobileBindingsError::Closed)
        ));
        assert!(matches!(native.dispose(), Err(MobileBindingsError::Closed)));

        release_tx.send(()).unwrap();
        let conversations = paused.join().unwrap().unwrap();
        assert!(
            conversations
                .iter()
                .any(|item| item.conversation_id.to_string() == captured.conversation_id)
        );
        let _ = std::fs::remove_file(path);
    }

    fn open_at(path: &std::path::Path, vault_id: &str, device_id: &str) -> Arc<NativeClient> {
        open_native_client(NativeOpenConfig {
            database_path: path.to_string_lossy().into_owned(),
            vault_id: vault_id.into(),
            device_id: device_id.into(),
            database_key: vec![9; 32],
        })
        .unwrap()
    }

    /// Opens a SQLCipher client and captures `count` distinct SMS while locked.
    fn locked_client_with_captures(
        path: &std::path::Path,
        vault_id: &str,
        device_id: &str,
        count: usize,
    ) -> Arc<NativeClient> {
        let client = open_at(path, vault_id, device_id);
        for index in 0..count {
            let captured = client
                .capture_incoming(NativeIncomingSms {
                    conversation_id: None,
                    sender_address: "+15550002222".into(),
                    body: format!("locked capture {index}"),
                    provider_message_id: Some(format!("locked-{index}")),
                    imported: false,
                })
                .unwrap();
            assert!(!captured.duplicate);
        }
        client
    }

    fn envelope_id(wire: &str) -> String {
        serde_json::from_str::<serde_json::Value>(wire).unwrap()["envelope_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn bounded_outbox_batch_seals_captures_beyond_implicit_unlock_batch() {
        const CAPTURES: usize = MAX_SEAL_BATCH + 44;
        const LIMIT: u64 = 20;
        let vault_id = uuid::Uuid::new_v4().to_string();
        let root = std::env::temp_dir().join(format!("peppy-mobile-seal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let material =
            create_smoke_vault_material(vault_id.clone(), "test passphrase".into()).unwrap();
        let unlock = |client: &NativeClient| {
            client
                .unlock(
                    material.profile_json.clone(),
                    material.header_json.clone(),
                    "test passphrase".into(),
                )
                .unwrap();
        };

        // Locked with nothing sealed: the batch is empty and sealing is a no-op.
        let draining = locked_client_with_captures(
            &root.join("drain.db"),
            &vault_id,
            &uuid::Uuid::new_v4().to_string(),
            CAPTURES,
        );
        assert!(draining.pending_outbox_json_batch(500).unwrap().is_empty());

        // One unlock seals only the implicit batch; limit 0 stays a no-op.
        unlock(&draining);
        assert_eq!(
            draining.pending_outbox_json().unwrap().len(),
            MAX_SEAL_BATCH
        );
        assert!(draining.pending_outbox_json_batch(0).unwrap().is_empty());
        assert_eq!(
            draining.pending_outbox_json().unwrap().len(),
            MAX_SEAL_BATCH
        );

        // Repeated bounded batches expose every capture without another unlock.
        let mut seen = std::collections::HashMap::new();
        for _ in 0..(CAPTURES / LIMIT as usize + 10) {
            let batch = draining.pending_outbox_json_batch(LIMIT).unwrap();
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= LIMIT as usize);
            assert_eq!(draining.pending_outbox_json_batch(LIMIT).unwrap(), batch);
            for wire in batch {
                let id = envelope_id(&wire);
                draining.ack_outbox(id.clone()).unwrap();
                assert!(seen.insert(id, wire).is_none(), "envelope exposed twice");
            }
        }
        assert_eq!(seen.len(), CAPTURES);
        assert!(draining.pending_outbox_json_batch(500).unwrap().is_empty());
        let distinct_wires: std::collections::HashSet<_> = seen.values().collect();
        assert_eq!(distinct_wires.len(), CAPTURES);

        // Sealed rows remain uploadable from a fresh, still-locked handle,
        // byte-identically, and the locked batch seals nothing further.
        let locked_path = root.join("locked.db");
        let device_id = uuid::Uuid::new_v4().to_string();
        let sealed = locked_client_with_captures(&locked_path, &vault_id, &device_id, CAPTURES);
        unlock(&sealed);
        let before = sealed.pending_outbox_json().unwrap();
        assert_eq!(before.len(), MAX_SEAL_BATCH);
        // Dropping the only handle closes the store, so the reopen has no keys.
        sealed.dispose().unwrap();
        drop(sealed);
        let reopened = open_at(&locked_path, &vault_id, &device_id);
        assert!(reopened.pending_outbox_json_batch(0).unwrap().is_empty());
        let first = reopened.pending_outbox_json_batch(500).unwrap();
        assert_eq!(first, before);
        assert_eq!(reopened.pending_outbox_json_batch(500).unwrap(), before);
        assert_eq!(reopened.pending_outbox_json().unwrap(), before);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn two_clients_sync_ack_and_execute_only_a_granted_permit() {
        let vault_id = uuid::Uuid::new_v4().to_string();
        let desktop_id = uuid::Uuid::new_v4().to_string();
        let gateway_id = uuid::Uuid::new_v4().to_string();
        let root =
            std::env::temp_dir().join(format!("peppy-native-facade-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let desktop_path = root.join("desktop.db");
        let gateway_path = root.join("gateway.db");
        let material =
            create_smoke_vault_material(vault_id.clone(), "test passphrase".into()).unwrap();
        let open = |path: &std::path::Path, device_id: String| {
            open_native_client(NativeOpenConfig {
                database_path: path.to_string_lossy().into_owned(),
                vault_id: vault_id.clone(),
                device_id,
                database_key: vec![3; 32],
            })
            .unwrap()
        };
        let desktop = open(&desktop_path, desktop_id);
        let gateway = open(&gateway_path, gateway_id.clone());
        for client in [&desktop, &gateway] {
            client
                .unlock(
                    material.profile_json.clone(),
                    material.header_json.clone(),
                    "test passphrase".into(),
                )
                .unwrap();
        }
        // The already-active verified epoch is an idempotent native cutover;
        // an unverified/unlocked higher epoch is rejected by the shared core.
        desktop.activate_verified_epoch(1).unwrap();
        assert!(matches!(
            desktop.activate_verified_epoch(2),
            Err(MobileBindingsError::InvalidProfile)
        ));

        let captured = gateway
            .capture_incoming(NativeIncomingSms {
                conversation_id: None,
                sender_address: "+15557654321".into(),
                body: "captured through native facade".into(),
                provider_message_id: Some("gateway-capture-1".into()),
                imported: false,
            })
            .unwrap();
        assert!(!captured.duplicate);

        let draft = desktop.create_compose_draft(None).unwrap();
        let route = serde_json::json!({
            "gateway_device_id": gateway_id,
            "subscription_id": "test-sim",
        })
        .to_string();
        let saved = desktop
            .save_compose_draft(
                draft.draft_id,
                draft.revision,
                NativeComposeDraftUpdate {
                    text: "gateway permit smoke".into(),
                    recipients: vec!["+15551234567".into()],
                    attachment_ids: vec![],
                    route_json: Some(route),
                },
            )
            .unwrap();
        let queued = desktop
            .send_compose_draft(saved.draft_id, saved.revision)
            .unwrap();
        assert!(desktop.pending_outbox_json_batch(0).unwrap().is_empty());
        assert_eq!(desktop.pending_outbox_json_batch(10_000).unwrap().len(), 1);
        let wire = desktop.pending_outbox_json().unwrap().pop().unwrap();
        desktop.ack_outbox(queued.envelope_id).unwrap();

        assert_eq!(
            gateway
                .ingest_raw(wire.into_bytes(), "1".into())
                .unwrap()
                .state,
            NativeIngestState::Journaled
        );
        gateway.apply_pending(1).unwrap();
        let command = gateway.pending_commands().unwrap().pop().unwrap();
        assert_eq!(
            gateway
                .pending_command_attachment_count(command.command_id.clone())
                .unwrap(),
            0
        );
        assert!(matches!(
            gateway.pending_command_attachment_count(uuid::Uuid::new_v4().to_string()),
            Err(MobileBindingsError::NotFound)
        ));
        assert!(matches!(
            gateway.pending_command_attachment_count("not-a-command-id".into()),
            Err(MobileBindingsError::InvalidRequest)
        ));
        let permit = gateway
            .begin_send_attempt(command.command_id.clone())
            .unwrap();
        assert_eq!(permit.state, NativePermitState::Permit);
        assert!(permit.command.is_some());
        assert_eq!(
            gateway
                .record_send_result(command.command_id.clone(), NativeSendResult::Sent)
                .unwrap(),
            "Sent"
        );
        assert_eq!(
            gateway
                .begin_send_attempt(command.command_id)
                .unwrap()
                .state,
            NativePermitState::AlreadyAttempted
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

uniffi::setup_scaffolding!();
