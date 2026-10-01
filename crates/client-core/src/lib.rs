//! The only SQLite/SQLCipher state owner used by native clients.
//!
//! SMS checkpoint contract (see `reports/C-final.md`):
//! * Every local write (capture, send command, send status, read state) is committed together with
//!   its outbox row. If the shared vault keys are locked the row stays `unsealed` and is encrypted
//!   exactly once when the active epoch is unlocked; sealed wires are retried byte-identically.
//! * Server data is journaled by cursor before any decryption. The receive cursor is the highest
//!   contiguous journaled cursor. Poison records are quarantined so they never wedge the stream.
//! * A gateway may only permit carrier sends for commands in its received-command ledger. A
//!   permit is durable before it is returned; after reopen an unfinished attempt is
//!   `OutcomeUnknown` and never yields a second permit.
//! * Native hosts may persist purpose keys only through `NativeKeyCache` in OS secure storage
//!   (Android Keystore-wrapped storage, iOS Keychain, desktop secure store); never JS or files.
//! * `send_compose_draft` queues one command and clears the draft in a single transaction.
//! * MMS media is encrypted with a fresh file key into core-owned files (see `media`). Outbox rows
//!   that reference media stay unsealed until every object has a remote ID; a gateway receives no
//!   carrier permit until every referenced object is locally verified.
//! * Snapshot resync is staged (`begin_snapshot` / `append_snapshot_page` / `finish_snapshot`) and
//!   merged monotonically; commands learned from a snapshot are historical and never permitted.
//!   A durable restore guard blocks all new carrier permits on a restored gateway; there is no
//!   clear. Rolling a database back behind the application is not detectable.
mod media;
use media::STREAM_VERSION;
pub use media::{
    AttachmentInfo, AttachmentState, CipherObject, MediaDescriptor, NativePlaintextFile,
};
use openpush_crypto::FileKey;
use openpush_crypto::{
    CryptoError, EncryptedEnvelope, KeyPurpose, PurposeKey, decrypt, derive_purpose_key,
    derive_root_key, encrypt, verify_vault_check_header,
};
pub use openpush_crypto::{KeyProfile, VaultCheckHeader};
pub use openpush_domain::{
    AttachmentId, AttachmentReference, CommandId, ConversationId, Cursor, DeviceId, DraftId,
    EnvelopeId, MessageId, MessageRecord, SendState, SourceSequence, VaultId,
};
pub use openpush_protocol::{Envelope, EnvelopePurpose, GatewayRoute};
use openpush_protocol::{MAX_CIPHERTEXT_BYTES, PROTOCOL_VERSION};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
};
use thiserror::Error;
use zeroize::Zeroizing;

const SCHEMA_VERSION: i64 = 8;
/// Records per `append_snapshot_page` call.
pub const MAX_SNAPSHOT_PAGE: usize = 500;
/// Records per staged snapshot generation.
pub const MAX_SNAPSHOT_RECORDS: u64 = 100_000;
/// Input bytes per snapshot page (typed records count their canonical JSON).
pub const MAX_SNAPSHOT_PAGE_BYTES: usize = 8 * 1024 * 1024;
/// Stored bytes per staged generation (oversize records are kept as small digest markers).
pub const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
/// Obsolete staging rows removed per maintenance step.
const SNAPSHOT_GC_BATCH: i64 = 2_000;
/// Upper bound applied to `apply_pending(limit)`.
pub const MAX_APPLY_BATCH: usize = 1_000;
/// Outbox rows sealed implicitly by enqueue/unlock/import/upload; use `seal_pending_batch` for more.
pub const MAX_SEAL_BATCH: usize = 256;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_ADDRESS_BYTES: usize = 256;
const MAX_PROVIDER_ID_BYTES: usize = 256;
const MAX_SUBSCRIPTION_BYTES: usize = 512;
const MAX_RAW_ENVELOPE_BYTES: usize = 2 * 1024 * 1024;
const NONCE_BYTES: usize = 24;
const AEAD_FRAME_MIN_BYTES: usize = NONCE_BYTES + 16;
const MAX_DRAFT_RECIPIENTS: usize = 20;
const MAX_MMS_RECIPIENTS: usize = 20;
const MAX_MMS_ATTACHMENTS: usize = 10;
const MAX_DRAFT_ATTACHMENTS: usize = MAX_MMS_ATTACHMENTS;
const KEY_CACHE_MAGIC: &[u8; 4] = b"OPKC";
const KEY_CACHE_VERSION: u8 = 1;
const FINGERPRINT_HEX_BYTES: usize = 64;
// magic | version | vault | device | epoch | profile fingerprint (hex) | command key | event key
const KEY_CACHE_BYTES: usize = 4 + 1 + 16 + 16 + 4 + FINGERPRINT_HEX_BYTES + 32 + 32;
const KEY_CACHE_CHECK_DOMAIN: &[u8] = b"openpush-native-key-cache-check-v1\0";

type Registry = Mutex<HashMap<PathBuf, Weak<Mutex<Store>>>>;
static REGISTRY: OnceLock<Registry> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    pub database_path: PathBuf,
    pub vault_id: VaultId,
    pub device_id: DeviceId,
}

/// The random, secure-store supplied 32-byte SQLCipher key. It is independent of the passphrase.
pub struct DatabaseKey(Zeroizing<[u8; 32]>);
impl DatabaseKey {
    pub fn new(bytes: &[u8]) -> Result<Self, Error> {
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| Error::InvalidDatabaseKey)?;
        Ok(Self(Zeroizing::new(bytes)))
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum Error {
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
    #[error("passphrase must not have surrounding whitespace")]
    InvalidPassphrase,
    #[error("key profile is not the pinned profile for this vault and epoch")]
    InvalidProfile,
    #[error("cryptographic operation failed")]
    Crypto,
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("cursor must be between 1 and 2^63-1")]
    InvalidCursor,
    #[error("a different record is already journaled at this cursor")]
    Conflict,
    #[error("not found")]
    NotFound,
    #[error("stale draft; current revision is {current_revision}")]
    StaleDraft { current_revision: u64 },
    #[error("no send attempt was recorded for this command")]
    NoAttempt,
    #[error("illegal send-state transition {from:?} -> {to:?}")]
    IllegalTransition { from: SendState, to: SendState },
    #[error("native key cache is malformed, altered, or was not recorded by this device")]
    InvalidKeyCache,
    #[error("keys for this epoch are not unlocked")]
    KeysUnavailable,
    #[error("attachment ciphertext failed length, digest or stream verification")]
    InvalidMedia,
    #[error("local media storage is unavailable")]
    Storage,
    #[error("snapshot import is inconsistent or no longer current; live state is unchanged")]
    SnapshotMismatch,
}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::Database
    }
}

/// Canonical private message record. It wraps D's `MessageRecord` and is only ever carried
/// inside AEAD ciphertext.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessagePayload {
    #[serde(flatten)]
    pub record: MessageRecord,
    pub source_device_id: DeviceId,
    pub provider_message_id: Option<String>,
    pub sender_address: Option<String>,
    pub recipients: Vec<String>,
    pub body: String,
    pub transport: Transport,
    pub direction: Direction,
    /// Imported provider history starts read; live captures start unread.
    pub imported: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Sms,
    Mms,
    Rcs,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Incoming,
    Outgoing,
}
/// Canonical encrypted inner wire. Native code never parses this; it only moves envelopes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrivatePayload {
    Message(MessagePayload),
    SendCommand {
        message: MessagePayload,
    },
    /// `media` matches `message.record.attachments` one-to-one, in order.
    MmsMessage {
        message: MessagePayload,
        media: Vec<MediaDescriptor>,
    },
    SendMmsCommand {
        message: MessagePayload,
        media: Vec<MediaDescriptor>,
    },
    SendStatus {
        command_id: CommandId,
        state: SendState,
    },
    ReadState {
        message_id: MessageId,
    },
}

/// A carrier SMS observed by a gateway. `conversation_id: None` resolves by exact sender address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingSms {
    pub conversation_id: Option<ConversationId>,
    pub sender_address: String,
    pub body: String,
    /// Stable provider identifier used to de-duplicate broadcast/provider re-captures.
    pub provider_message_id: Option<String>,
    pub imported: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captured {
    pub message_id: MessageId,
    pub conversation_id: ConversationId,
    /// True when `provider_message_id` was already captured; nothing new was queued.
    pub duplicate: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingSms {
    pub conversation_id: ConversationId,
    /// Exactly one recipient for SMS.
    pub recipients: Vec<String>,
    pub body: String,
}
/// A carrier MMS observed by a gateway; attachments come from `prepare_attachment`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingMms {
    pub conversation_id: Option<ConversationId>,
    pub sender_address: String,
    pub body: String,
    pub provider_message_id: Option<String>,
    pub imported: bool,
    pub attachment_ids: Vec<AttachmentId>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingMms {
    pub conversation_id: ConversationId,
    /// 1..=20 recipients.
    pub recipients: Vec<String>,
    /// May be empty when attachments are present.
    pub body: String,
    /// 1..=10 attachments prepared on this device.
    pub attachment_ids: Vec<AttachmentId>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedSend {
    pub message_id: MessageId,
    pub command_id: CommandId,
    pub envelope_id: EnvelopeId,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub payload: MessagePayload,
    pub seen: bool,
    /// `None` for messages that are not send commands.
    pub send_state: Option<SendState>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub conversation_id: ConversationId,
    pub unread_count: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub conversation_id: ConversationId,
    pub content: String,
    pub revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyStatus {
    pub active_epoch: Option<u32>,
    pub unlocked_epochs: Vec<u32>,
}
/// A decrypted, authorized command this gateway may carry out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedCommand {
    pub command_id: CommandId,
    pub subscription_id: String,
    pub message: MessagePayload,
}
// Returned once per carrier send; a flat record keeps the future UniFFI enum simple.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermitDecision {
    /// The attempt is durably recorded; the native host may submit once to the OS.
    Permit(ReceivedCommand),
    /// An attempt already exists. `OutcomeUnknown` requires reconciliation, never a resend.
    AlreadyAttempted(SendState),
    Blocked(PermitBlock),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermitBlock {
    /// Not in this gateway's received-command ledger (unknown or routed elsewhere).
    NotReceived,
    /// Encrypted under a retired epoch; manual cutover disables it.
    StaleEpoch,
    /// Encrypted under an epoch this gateway has not activated.
    EpochNotActive,
    /// An MMS command references media that is not yet downloaded and verified locally.
    MediaUnavailable,
    /// Learned from a snapshot/history import; never executable, requires reconciliation.
    Historical,
    /// This gateway was restored from backup/import; no new carrier work may start.
    RestoreGuarded,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendResult {
    Submitted,
    Sent,
    Delivered,
    FailedBeforeSubmission,
    FailedConfirmed,
}
impl From<SendResult> for SendState {
    fn from(value: SendResult) -> Self {
        match value {
            SendResult::Submitted => Self::SubmittedToOs,
            SendResult::Sent => Self::Sent,
            SendResult::Delivered => Self::Delivered,
            SendResult::FailedBeforeSubmission => Self::FailedBeforeSubmission,
            SendResult::FailedConfirmed => Self::FailedConfirmed,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestResult {
    /// Durably journaled; applied by `apply_pending` once contiguous and keys are available.
    Journaled,
    /// Identical replay (same cursor, or same envelope at another cursor, or this device's echo).
    Duplicate,
    /// Journaled as a visible poison record; the receive cursor still advances.
    Quarantined(QuarantineReason),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarantineReason {
    MalformedEnvelope,
    WrongVault,
    EnvelopeConflict,
    ProfileMismatch,
    AuthenticationFailed,
    InvalidPayload,
    PayloadConflict,
}
impl QuarantineReason {
    fn code(self) -> &'static str {
        match self {
            Self::MalformedEnvelope => "malformed_envelope",
            Self::WrongVault => "wrong_vault",
            Self::EnvelopeConflict => "envelope_conflict",
            Self::ProfileMismatch => "profile_mismatch",
            Self::AuthenticationFailed => "authentication_failed",
            Self::InvalidPayload => "invalid_payload",
            Self::PayloadConflict => "payload_conflict",
        }
    }
    fn from_code(code: &str) -> Result<Self, Error> {
        [
            Self::MalformedEnvelope,
            Self::WrongVault,
            Self::EnvelopeConflict,
            Self::ProfileMismatch,
            Self::AuthenticationFailed,
            Self::InvalidPayload,
            Self::PayloadConflict,
        ]
        .into_iter()
        .find(|reason| reason.code() == code)
        .ok_or(Error::Database)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantinedRecord {
    pub cursor: Cursor,
    pub envelope_id: Option<EnvelopeId>,
    pub reason: QuarantineReason,
}
/// One immutable encrypted record from the server snapshot (`{cursor, envelope}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub cursor: Cursor,
    pub envelope: Envelope,
}
/// A snapshot record as raw server JSON; canonicalized exactly like `ingest_raw`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawSnapshotRecord {
    pub cursor: Cursor,
    pub envelope_json: Vec<u8>,
}
/// A local outbox row whose envelope ID the server accepted with different bytes (for example
/// after a database rollback). It is never uploaded or sealed again; reconcile it explicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxConflict {
    pub envelope_id: EnvelopeId,
    pub command_id: Option<CommandId>,
    pub cursor: Cursor,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotPurpose {
    /// Cursor expired or new device: merge history only.
    Resync,
    /// Gateway restored from a backup/import: also sets the durable restore guard at begin.
    Restore,
}
/// A staged import. `generation` must accompany every page and the finish call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotProgress {
    pub generation: u64,
    pub high_water: Cursor,
    pub expected_records: u64,
    pub received_records: u64,
    pub last_cursor: Cursor,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotReport {
    /// Published records still to be drained into the journal by `apply_pending`.
    pub journaled: usize,
    /// Records identical to rows already journaled at the same cursor.
    pub duplicate: usize,
    /// Always 0 at publish; invalid records are quarantined while draining.
    pub quarantined: usize,
    /// Receive cursor after publish (>= snapshot high-water).
    pub receive_cursor: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconciliationReason {
    Historical,
    RestoreGuarded,
    OutcomeUnknown,
}
/// A received command that must be reconciled with OS/provider evidence, never re-executed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationItem {
    pub command_id: CommandId,
    pub message: MessagePayload,
    pub reason: ReconciliationReason,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied: usize,
    pub quarantined: usize,
    /// Contiguous pending records whose key epoch is not unlocked on this device.
    pub waiting_for_keys: usize,
    /// Published snapshot records moved into the journal by this call.
    pub drained: usize,
    /// Published snapshot records still waiting to be drained; keep calling while > 0.
    pub snapshot_remaining: u64,
}

/// Opaque purpose-key cache for native secure storage only. It deliberately implements no
/// `Debug`, `Clone` or `Serialize`; never place it in UI view models, JS, logs or plain files.
/// Bound to format version, vault, device, epoch, profile fingerprint and both purposes.
pub struct NativeKeyCache(Zeroizing<Vec<u8>>);
impl NativeKeyCache {
    /// Wraps bytes read back from OS secure storage. Validation happens on import.
    pub fn from_native_storage(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }
    /// Bytes to hand directly to OS secure storage.
    pub fn native_storage_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Durable composer state. SMS sends require exactly one recipient, a route, and no attachments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposeDraft {
    pub draft_id: DraftId,
    pub conversation_id: ConversationId,
    pub text: String,
    pub recipients: Vec<String>,
    pub attachment_ids: Vec<AttachmentId>,
    pub route: Option<GatewayRoute>,
    pub revision: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComposeDraftUpdate {
    pub text: String,
    pub recipients: Vec<String>,
    pub attachment_ids: Vec<AttachmentId>,
    pub route: Option<GatewayRoute>,
}

struct EpochKeys {
    profile: KeyProfile,
    fingerprint: String,
    command: PurposeKey,
    event: PurposeKey,
}
struct Store {
    conn: Connection,
    config: ClientConfig,
    db_key: Zeroizing<[u8; 32]>,
    keys: BTreeMap<u32, EpochKeys>,
    active_epoch: Option<u32>,
    media_dir: PathBuf,
}
struct Ctx<'a> {
    vault_id: VaultId,
    device_id: DeviceId,
    keys: &'a BTreeMap<u32, EpochKeys>,
    active_epoch: Option<u32>,
}
impl Store {
    fn parts(&mut self) -> (&mut Connection, Ctx<'_>) {
        (
            &mut self.conn,
            Ctx {
                vault_id: self.config.vault_id,
                device_id: self.config.device_id,
                keys: &self.keys,
                active_epoch: self.active_epoch,
            },
        )
    }
}

/// Cloneable handle to the process-wide owner of one database path.
#[derive(Clone)]
pub struct Client(Arc<Mutex<Store>>);

impl Client {
    /// Opens (or shares) the per-canonical-path owner. A second open must present the same key
    /// and identity. A wrong key is reported, never treated as a new database.
    pub fn open(config: ClientConfig, db_key: DatabaseKey) -> Result<Self, Error> {
        let path = canonical(&config.database_path)?;
        let mut registry = REGISTRY
            .get_or_init(Registry::default)
            .lock()
            .map_err(|_| Error::Database)?;
        if let Some(store) = registry.get(&path).and_then(Weak::upgrade) {
            let s = store.lock().map_err(|_| Error::Database)?;
            if !same_key(&s.db_key, &db_key.0) {
                return Err(Error::WrongDatabaseKey);
            }
            if s.config.vault_id != config.vault_id || s.config.device_id != config.device_id {
                return Err(Error::IdentityMismatch);
            }
            drop(s);
            return Ok(Self(store));
        }
        registry.retain(|_, weak| weak.strong_count() > 0);
        let conn = Connection::open(&path)?;
        apply_database_key(&conn, &db_key)?;
        let media_dir = media::prepare_media_root(&path)?;
        let mut store = Store {
            conn,
            config,
            db_key: Zeroizing::new(*db_key.0),
            keys: BTreeMap::new(),
            active_epoch: None,
            media_dir,
        };
        initialize(&mut store)?;
        let store = Arc::new(Mutex::new(store));
        registry.insert(path, Arc::downgrade(&store));
        Ok(Self(store))
    }

    /// Verifies the passphrase against the authenticated vault header, pins the profile for its
    /// epoch on first use, and seals any outbox rows queued while locked. The KDF runs outside
    /// the database lock.
    pub fn unlock(
        &self,
        profile: &KeyProfile,
        header: &VaultCheckHeader,
        passphrase: &str,
    ) -> Result<(), Error> {
        {
            let s = self.lock()?;
            if profile.vault_id != s.config.vault_id.0 || header.profile != *profile {
                return Err(Error::InvalidProfile);
            }
            if pinned_profile(&s.conn, profile.key_epoch)?.is_some_and(|pinned| pinned != *profile)
            {
                return Err(Error::InvalidProfile);
            }
        }
        let root = derive_root_key(passphrase, profile).map_err(|error| match error {
            CryptoError::SurroundingWhitespace => Error::InvalidPassphrase,
            CryptoError::UnsupportedSuite | CryptoError::InvalidProfile => Error::InvalidProfile,
            _ => Error::Crypto,
        })?;
        verify_vault_check_header(&root, profile, header).map_err(|error| match error {
            CryptoError::AuthenticationFailed => Error::WrongPassphrase,
            CryptoError::InvalidProfile => Error::InvalidProfile,
            _ => Error::Crypto,
        })?;
        let derive =
            |purpose| derive_purpose_key(&root, profile, purpose).map_err(|_| Error::Crypto);
        let keys = EpochKeys {
            profile: profile.clone(),
            fingerprint: profile.fingerprint().map_err(|_| Error::Crypto)?,
            command: derive(KeyPurpose::Command)?,
            event: derive(KeyPurpose::Event)?,
        };
        let mut guard = self.lock()?;
        let s = &mut *guard;
        let tx = s
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        match pinned_profile(&tx, profile.key_epoch)? {
            Some(pinned) if pinned != *profile => return Err(Error::InvalidProfile),
            Some(_) => {}
            None => {
                tx.execute(
                    "INSERT INTO key_epoch_profiles(epoch,profile) VALUES(?,?)",
                    params![profile.key_epoch, json(profile)?],
                )?;
            }
        }
        let active = match s.active_epoch {
            Some(active) => active,
            None => {
                set_meta(&tx, "active_epoch", &profile.key_epoch.to_string())?;
                profile.key_epoch
            }
        };
        // Integrity reference for later native-cache imports; stays inside SQLCipher.
        let check = key_cache_check(&key_cache_bytes(&s.config, profile.key_epoch, &keys)?);
        tx.execute(
            "INSERT INTO key_cache_checks(epoch,check_value) VALUES(?,?) ON CONFLICT(epoch) DO UPDATE SET check_value=excluded.check_value",
            params![profile.key_epoch, check.as_slice()],
        )?;
        tx.commit()?;
        s.active_epoch = Some(active);
        s.keys.insert(profile.key_epoch, keys);
        seal_with_store(s)
    }

    /// Manual epoch cutover. Requires the newer epoch to be unlocked; older epochs stay
    /// decrypt-only. In the same transaction, every received older-epoch command that was never
    /// attempted is retired with one encrypted `FailedBeforeSubmission` status. Commands that
    /// already have an attempt keep their recorded outcome.
    pub fn activate_epoch(&self, epoch: u32) -> Result<(), Error> {
        let mut guard = self.lock()?;
        let s = &mut *guard;
        if !s.keys.contains_key(&epoch) || s.active_epoch.is_some_and(|old| epoch <= old) {
            return Err(Error::InvalidProfile);
        }
        let ctx = Ctx {
            vault_id: s.config.vault_id,
            device_id: s.config.device_id,
            keys: &s.keys,
            active_epoch: Some(epoch),
        };
        let tx = s
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        set_meta(&tx, "active_epoch", &epoch.to_string())?;
        let retired: Vec<String> = {
            let mut query = tx.prepare(
                "SELECT l.command_id FROM command_ledger l LEFT JOIN attempts a ON a.command_id=l.command_id
                 WHERE a.command_id IS NULL AND l.retired=0 AND l.historical=0 AND l.key_epoch<? ORDER BY l.local_order",
            )?;
            query
                .query_map(params![epoch], |r| r.get(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for id in retired {
            tx.execute(
                "UPDATE command_ledger SET retired=1 WHERE command_id=?",
                params![id],
            )?;
            emit_status(&tx, &ctx, parse(&id)?, SendState::FailedBeforeSubmission)?;
        }
        seal_pending(&tx, &ctx, MAX_SEAL_BATCH)?;
        tx.commit()?;
        s.active_epoch = Some(epoch);
        Ok(())
    }

    /// Exports both purpose keys of an unlocked epoch for OS secure storage. The root key and
    /// passphrase are never exported.
    pub fn export_native_key_cache(&self, epoch: u32) -> Result<NativeKeyCache, Error> {
        let s = self.lock()?;
        let keys = s.keys.get(&epoch).ok_or(Error::KeysUnavailable)?;
        Ok(NativeKeyCache(key_cache_bytes(&s.config, epoch, keys)?))
    }

    /// Restores purpose keys without a passphrase. Requires this device's pinned profile for
    /// the cached epoch and the integrity check recorded at passphrase unlock; it never pins a
    /// profile or changes the active epoch. On any failure installed keys and the outbox are
    /// unchanged. Seals locked work when the active epoch becomes available.
    pub fn import_native_key_cache(&self, cache: &NativeKeyCache) -> Result<(), Error> {
        let bytes = cache.native_storage_bytes();
        if bytes.len() != KEY_CACHE_BYTES
            || &bytes[..4] != KEY_CACHE_MAGIC
            || bytes[4] != KEY_CACHE_VERSION
        {
            return Err(Error::InvalidKeyCache);
        }
        let field = |start: usize, len: usize| &bytes[start..start + len];
        let uuid = |start| {
            <[u8; 16]>::try_from(field(start, 16))
                .map(uuid::Uuid::from_bytes)
                .map_err(|_| Error::InvalidKeyCache)
        };
        let (vault, device) = (VaultId(uuid(5)?), DeviceId(uuid(21)?));
        let epoch = u32::from_be_bytes(
            field(37, 4)
                .try_into()
                .map_err(|_| Error::InvalidKeyCache)?,
        );
        let fingerprint = std::str::from_utf8(field(41, FINGERPRINT_HEX_BYTES))
            .map_err(|_| Error::InvalidKeyCache)?;
        let key = |start| -> Result<Zeroizing<[u8; 32]>, Error> {
            Ok(Zeroizing::new(
                field(start, 32)
                    .try_into()
                    .map_err(|_| Error::InvalidKeyCache)?,
            ))
        };
        let (command, event) = (key(105)?, key(137)?);
        let mut guard = self.lock()?;
        let s = &mut *guard;
        if vault != s.config.vault_id || device != s.config.device_id {
            return Err(Error::IdentityMismatch);
        }
        let profile = pinned_profile(&s.conn, epoch)?.ok_or(Error::InvalidProfile)?;
        if profile.fingerprint().map_err(|_| Error::Crypto)? != fingerprint {
            return Err(Error::InvalidProfile);
        }
        let recorded: Vec<u8> = s
            .conn
            .query_row(
                "SELECT check_value FROM key_cache_checks WHERE epoch=?",
                params![epoch],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::InvalidKeyCache)?;
        if !same_bytes(&recorded, &key_cache_check(bytes)) {
            return Err(Error::InvalidKeyCache);
        }
        let import = |bytes: &[u8; 32], purpose| {
            PurposeKey::import_native_cache(*bytes, &profile, purpose).map_err(|_| Error::Crypto)
        };
        let keys = EpochKeys {
            fingerprint: fingerprint.to_owned(),
            command: import(&command, KeyPurpose::Command)?,
            event: import(&event, KeyPurpose::Event)?,
            profile: profile.clone(),
        };
        s.keys.insert(epoch, keys);
        seal_with_store(s)
    }

    pub fn key_status(&self) -> Result<KeyStatus, Error> {
        let s = self.lock()?;
        Ok(KeyStatus {
            active_epoch: s.active_epoch,
            unlocked_epochs: s.keys.keys().copied().collect(),
        })
    }

    /// Durably captures a carrier SMS (works while vault keys are locked).
    pub fn capture_incoming(&self, sms: IncomingSms) -> Result<Captured, Error> {
        self.capture(sms, &[])
    }

    /// Durably captures a carrier MMS whose parts were already passed to `prepare_attachment`.
    /// The event stays unsealed until every attachment is uploaded.
    pub fn capture_incoming_mms(&self, mms: IncomingMms) -> Result<Captured, Error> {
        if mms.attachment_ids.is_empty() || mms.attachment_ids.len() > MAX_MMS_ATTACHMENTS {
            return Err(Error::InvalidRequest("MMS requires 1..=10 attachments"));
        }
        let attachments = mms.attachment_ids;
        self.capture(
            IncomingSms {
                conversation_id: mms.conversation_id,
                sender_address: mms.sender_address,
                body: mms.body,
                provider_message_id: mms.provider_message_id,
                imported: mms.imported,
            },
            &attachments,
        )
    }

    fn capture(&self, sms: IncomingSms, attachments: &[AttachmentId]) -> Result<Captured, Error> {
        require_address(&sms.sender_address)?;
        require_body(&sms.body, true)?;
        if let Some(id) = &sms.provider_message_id
            && (id.is_empty() || id.len() > MAX_PROVIDER_ID_BYTES)
        {
            return Err(Error::InvalidRequest("provider message id"));
        }
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(provider_id) = &sms.provider_message_id {
            let existing: Option<(String, String)> = tx
                .query_row(
                    "SELECT id,conversation_id FROM messages WHERE source_device_id=? AND provider_message_id=?",
                    params![ctx.device_id.to_string(), provider_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((message_id, conversation_id)) = existing {
                return Ok(Captured {
                    message_id: parse(&message_id)?,
                    conversation_id: parse(&conversation_id)?,
                    duplicate: true,
                });
            }
        }
        let conversation_id = match sms.conversation_id {
            Some(id) => id,
            None => tx
                .query_row(
                    "SELECT conversation_id FROM addresses WHERE address=?",
                    params![sms.sender_address],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .map(|id| parse(&id))
                .transpose()?
                .unwrap_or_else(ConversationId::new),
        };
        let payload = MessagePayload {
            record: MessageRecord {
                message_id: MessageId::new(),
                conversation_id,
                source_sequence: next_source_sequence(&tx, conversation_id, ctx.device_id)?,
                attachments: local_references(&tx, attachments)?,
            },
            source_device_id: ctx.device_id,
            provider_message_id: sms.provider_message_id,
            sender_address: Some(sms.sender_address),
            recipients: Vec::new(),
            body: sms.body,
            transport: if attachments.is_empty() {
                Transport::Sms
            } else {
                Transport::Mms
            },
            direction: Direction::Incoming,
            imported: sms.imported,
        };
        if insert_message(&tx, &payload)? != Inserted::New {
            return Err(Error::Database);
        }
        let message_id = payload.record.message_id;
        let private = if attachments.is_empty() {
            PrivatePayload::Message(payload)
        } else {
            PrivatePayload::MmsMessage {
                message: payload,
                media: Vec::new(),
            }
        };
        enqueue(&tx, &ctx, EnvelopePurpose::Event, None, None, &private)?;
        tx.commit()?;
        Ok(Captured {
            message_id,
            conversation_id,
            duplicate: false,
        })
    }

    /// Queues an encrypted send command pinned to one gateway and subscription.
    pub fn queue_send(&self, sms: OutgoingSms, route: GatewayRoute) -> Result<QueuedSend, Error> {
        validate_outgoing(&sms, &route)?;
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let queued = queue_sms(&tx, &ctx, sms, &route, &[])?;
        tx.commit()?;
        Ok(queued)
    }

    /// Queues an MMS command. The command is sealed once every attachment is uploaded.
    pub fn queue_mms(&self, mms: OutgoingMms, route: GatewayRoute) -> Result<QueuedSend, Error> {
        validate_outgoing_mms(&mms.recipients, &mms.body, &mms.attachment_ids, &route)?;
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sms = OutgoingSms {
            conversation_id: mms.conversation_id,
            recipients: mms.recipients,
            body: mms.body,
        };
        let queued = queue_sms(&tx, &ctx, sms, &route, &mms.attachment_ids)?;
        tx.commit()?;
        Ok(queued)
    }

    /// Creates a draft, or returns the existing draft for `conversation_id`. `None` starts a new
    /// conversation with a stable ID.
    pub fn create_compose_draft(
        &self,
        conversation_id: Option<ConversationId>,
    ) -> Result<ComposeDraft, Error> {
        let s = self.lock()?;
        if let Some(conversation_id) = conversation_id
            && let Some(existing) =
                load_compose_draft(&s.conn, "conversation_id", &conversation_id.to_string())?
        {
            return Ok(existing);
        }
        let draft = ComposeDraft {
            draft_id: DraftId::new(),
            conversation_id: conversation_id.unwrap_or_else(ConversationId::new),
            text: String::new(),
            recipients: Vec::new(),
            attachment_ids: Vec::new(),
            route: None,
            revision: 0,
        };
        s.conn.execute(
            "INSERT INTO compose_drafts(draft_id,conversation_id,text,recipients,attachment_ids,route,revision) VALUES(?,?,'','[]','[]',NULL,0)",
            params![draft.draft_id.to_string(), draft.conversation_id.to_string()],
        )?;
        Ok(draft)
    }

    pub fn compose_draft(&self, draft_id: DraftId) -> Result<Option<ComposeDraft>, Error> {
        load_compose_draft(&self.lock()?.conn, "draft_id", &draft_id.to_string())
    }

    /// All compose drafts, including draft-only conversations, in creation order.
    pub fn compose_drafts(&self) -> Result<Vec<ComposeDraft>, Error> {
        let s = self.lock()?;
        let mut query = s.conn.prepare(&format!(
            "SELECT {COMPOSE_COLUMNS} FROM compose_drafts ORDER BY local_order"
        ))?;
        let rows = query
            .query_map([], compose_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(decode_compose_row).collect()
    }

    /// Compare-and-swap save; a stale revision leaves stored content untouched.
    pub fn save_compose_draft(
        &self,
        draft_id: DraftId,
        expected_revision: u64,
        update: ComposeDraftUpdate,
    ) -> Result<ComposeDraft, Error> {
        require_body(&update.text, true)?;
        if update.recipients.len() > MAX_DRAFT_RECIPIENTS {
            return Err(Error::InvalidRequest("draft allows at most 20 recipients"));
        }
        if update.attachment_ids.len() > MAX_DRAFT_ATTACHMENTS {
            return Err(Error::InvalidRequest("draft allows at most 10 attachments"));
        }
        update
            .recipients
            .iter()
            .try_for_each(|a| require_address(a))?;
        if let Some(route) = &update.route {
            require_subscription(route)?;
        }
        let mut s = self.lock()?;
        let tx = s
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current =
            load_compose_draft(&tx, "draft_id", &draft_id.to_string())?.ok_or(Error::NotFound)?;
        if current.revision != expected_revision {
            return Err(Error::StaleDraft {
                current_revision: current.revision,
            });
        }
        let next = current.revision.checked_add(1).ok_or(Error::Database)?;
        tx.execute(
            "UPDATE compose_drafts SET text=?,recipients=?,attachment_ids=?,route=?,revision=? WHERE draft_id=?",
            params![
                update.text,
                json(&update.recipients)?,
                json(&update.attachment_ids)?,
                update.route.as_ref().map(json).transpose()?,
                to_i64(next)?,
                draft_id.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(ComposeDraft {
            draft_id,
            conversation_id: current.conversation_id,
            text: update.text,
            recipients: update.recipients,
            attachment_ids: update.attachment_ids,
            route: update.route,
            revision: next,
        })
    }

    /// Sends the stored draft at `expected_revision`: queues exactly one command, then clears
    /// text/attachments and bumps the revision in the same transaction. Recipients and route are
    /// kept. Replaying an old revision returns `StaleDraft` and queues nothing.
    pub fn send_compose_draft(
        &self,
        draft_id: DraftId,
        expected_revision: u64,
    ) -> Result<QueuedSend, Error> {
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let draft =
            load_compose_draft(&tx, "draft_id", &draft_id.to_string())?.ok_or(Error::NotFound)?;
        if draft.revision != expected_revision {
            return Err(Error::StaleDraft {
                current_revision: draft.revision,
            });
        }
        let route = draft.route.ok_or(Error::InvalidRequest("route"))?;
        // Stored attachment IDs select MMS; media metadata and keys come only from SQLCipher.
        if draft.attachment_ids.is_empty() {
            validate_outgoing(
                &OutgoingSms {
                    conversation_id: draft.conversation_id,
                    recipients: draft.recipients.clone(),
                    body: draft.text.clone(),
                },
                &route,
            )?;
        } else {
            validate_outgoing_mms(
                &draft.recipients,
                &draft.text,
                &draft.attachment_ids,
                &route,
            )?;
        }
        let sms = OutgoingSms {
            conversation_id: draft.conversation_id,
            recipients: draft.recipients,
            body: draft.text,
        };
        let queued = queue_sms(&tx, &ctx, sms, &route, &draft.attachment_ids)?;
        let next = draft.revision.checked_add(1).ok_or(Error::Database)?;
        tx.execute(
            "UPDATE compose_drafts SET text='',attachment_ids='[]',revision=? WHERE draft_id=?",
            params![to_i64(next)?, draft_id.to_string()],
        )?;
        tx.commit()?;
        Ok(queued)
    }
}

fn validate_outgoing(sms: &OutgoingSms, route: &GatewayRoute) -> Result<(), Error> {
    let [recipient] = sms.recipients.as_slice() else {
        return Err(Error::InvalidRequest("SMS requires exactly one recipient"));
    };
    require_address(recipient)?;
    require_body(&sms.body, false)?;
    require_subscription(route)
}
fn validate_outgoing_mms(
    recipients: &[String],
    body: &str,
    attachments: &[AttachmentId],
    route: &GatewayRoute,
) -> Result<(), Error> {
    if recipients.is_empty() || recipients.len() > MAX_MMS_RECIPIENTS {
        return Err(Error::InvalidRequest("MMS requires 1..=20 recipients"));
    }
    if attachments.is_empty() || attachments.len() > MAX_MMS_ATTACHMENTS {
        return Err(Error::InvalidRequest("MMS requires 1..=10 attachments"));
    }
    recipients.iter().try_for_each(|a| require_address(a))?;
    require_body(body, true)?;
    require_subscription(route)
}
fn require_subscription(route: &GatewayRoute) -> Result<(), Error> {
    if route.subscription_id.is_empty() || route.subscription_id.len() > MAX_SUBSCRIPTION_BYTES {
        return Err(Error::InvalidRequest("subscription id"));
    }
    Ok(())
}

/// Writes the outgoing message, command binding and command outbox row for a validated SMS.
/// Writes the outgoing message, command binding and command outbox row for a validated SMS
/// (no attachments) or MMS (attachments prepared on this device).
fn queue_sms(
    tx: &Connection,
    ctx: &Ctx,
    sms: OutgoingSms,
    route: &GatewayRoute,
    attachments: &[AttachmentId],
) -> Result<QueuedSend, Error> {
    let payload = MessagePayload {
        record: MessageRecord {
            message_id: MessageId::new(),
            conversation_id: sms.conversation_id,
            source_sequence: next_source_sequence(tx, sms.conversation_id, ctx.device_id)?,
            attachments: local_references(tx, attachments)?,
        },
        source_device_id: ctx.device_id,
        provider_message_id: None,
        sender_address: None,
        recipients: sms.recipients,
        body: sms.body,
        transport: if attachments.is_empty() {
            Transport::Sms
        } else {
            Transport::Mms
        },
        direction: Direction::Outgoing,
        imported: false,
    };
    let message_id = payload.record.message_id;
    let command_id = CommandId::new();
    if insert_message(tx, &payload)? != Inserted::New
        || !record_command(tx, command_id, message_id, route.gateway_device_id)?
    {
        return Err(Error::Database);
    }
    let private = if attachments.is_empty() {
        PrivatePayload::SendCommand { message: payload }
    } else {
        PrivatePayload::SendMmsCommand {
            message: payload,
            media: Vec::new(),
        }
    };
    let (envelope_id, _) = enqueue(
        tx,
        ctx,
        EnvelopePurpose::Command,
        Some(command_id),
        Some(route),
        &private,
    )?;
    Ok(QueuedSend {
        message_id,
        command_id,
        envelope_id,
    })
}

impl Client {
    /// All sealed, unacknowledged envelopes in producer-sequence order. Retries are
    /// byte-identical. Background hosts should prefer `pending_outbox_batch`.
    pub fn pending_outbox(&self) -> Result<Vec<Envelope>, Error> {
        self.pending_outbox_batch(usize::MAX)
    }

    /// At most `limit` of the oldest sealed, unacknowledged envelopes.
    pub fn pending_outbox_batch(&self, limit: usize) -> Result<Vec<Envelope>, Error> {
        let s = self.lock()?;
        let mut query = s
            .conn
            .prepare("SELECT wire FROM outbox WHERE state='queued' AND envelope_id NOT IN (SELECT envelope_id FROM outbox_conflicts) ORDER BY seq LIMIT ?")?;
        let wires = query
            .query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
                r.get::<_, Vec<u8>>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        wires.iter().map(|wire| decode(wire)).collect()
    }

    /// Records server acceptance of one sealed outbox envelope. Idempotent.
    pub fn ack_outbox(&self, envelope_id: EnvelopeId) -> Result<(), Error> {
        let s = self.lock()?;
        let changed = s.conn.execute(
            "UPDATE outbox SET state='acknowledged' WHERE envelope_id=? AND state IN ('queued','acknowledged')",
            params![envelope_id.to_string()],
        )?;
        if changed == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Journals one server event at `cursor` before any decryption.
    pub fn ingest(&self, envelope: &Envelope, cursor: Cursor) -> Result<IngestResult, Error> {
        let wire = typed_wire(envelope)?;
        let parsed = parse_wire(&wire);
        self.journal(cursor, wire, parsed.as_ref())
    }

    /// Like `ingest`, for raw server JSON. Unparseable bytes are quarantined at their cursor.
    pub fn ingest_raw(&self, envelope_json: &[u8], cursor: Cursor) -> Result<IngestResult, Error> {
        let wire = raw_wire(envelope_json)?;
        let parsed = parse_wire(&wire);
        self.journal(cursor, wire, parsed.as_ref())
    }

    /// Highest contiguous journaled cursor; resume server fetches after it.
    pub fn receive_cursor(&self) -> Result<Cursor, Error> {
        Ok(Cursor(receive_cursor(&self.lock()?.conn)?))
    }

    /// Drains up to `limit` published snapshot records into the journal (in cursor order), then
    /// decrypts and applies up to `limit` contiguous journaled records whose epoch is unlocked.
    /// While a snapshot is draining, nothing after the last drained cursor is applied, so a live
    /// tail never overtakes earlier history. Both steps are capped at `MAX_APPLY_BATCH`; call
    /// repeatedly while `applied > 0` or `snapshot_remaining > 0`.
    pub fn apply_pending(&self, limit: usize) -> Result<ApplyReport, Error> {
        let limit = limit.min(MAX_APPLY_BATCH);
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        let frontier = to_i64(receive_cursor(conn)?)?;
        let epochs = ctx
            .keys
            .keys()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut report = ApplyReport::default();
        collect_snapshot_garbage(&tx)?;
        let ceiling = match drain_snapshot(&tx, &ctx, limit, &mut report)? {
            Some(drained_through) => drained_through.min(frontier),
            None => frontier,
        };
        let rows: Vec<(i64, Vec<u8>, bool)> = if epochs.is_empty() {
            Vec::new()
        } else {
            let mut query = tx.prepare(&format!(
                "SELECT LENGTH(wire),cursor,wire,historical FROM journal WHERE status='pending' AND cursor<=?1 AND key_epoch IN ({epochs}) ORDER BY cursor LIMIT ?2"
            ))?;
            take_within_budget(
                query.query(params![ceiling, to_i64(limit as u64)?])?,
                MAX_SNAPSHOT_PAGE_BYTES,
                |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?)),
            )?
        };
        for (cursor, wire, historical) in rows {
            let savepoint = tx.savepoint()?;
            match apply_record(&savepoint, &ctx, &wire, historical)? {
                None => {
                    savepoint.commit()?;
                    tx.execute(
                        "UPDATE journal SET status='applied' WHERE cursor=?",
                        params![cursor],
                    )?;
                    report.applied += 1;
                }
                Some(reason) => {
                    drop(savepoint);
                    tx.execute(
                        "UPDATE journal SET status='quarantined',reason=? WHERE cursor=?",
                        params![reason.code(), cursor],
                    )?;
                    report.quarantined += 1;
                }
            }
        }
        let waiting: i64 = tx.query_row(
            &format!(
                "SELECT COUNT(*) FROM journal WHERE status='pending' AND cursor<=?1 AND key_epoch NOT IN ({epochs})"
            ),
            params![ceiling],
            |r| r.get(0),
        )?;
        report.waiting_for_keys = usize::try_from(waiting).map_err(|_| Error::Database)?;
        tx.commit()?;
        Ok(report)
    }

    pub fn quarantined(&self) -> Result<Vec<QuarantinedRecord>, Error> {
        let s = self.lock()?;
        let mut query = s.conn.prepare(
            "SELECT cursor,envelope_id,reason FROM journal WHERE status='quarantined' ORDER BY cursor",
        )?;
        let rows = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(cursor, envelope_id, reason)| {
                Ok(QuarantinedRecord {
                    cursor: Cursor(u64::try_from(cursor).map_err(|_| Error::Database)?),
                    envelope_id: envelope_id.map(|id| parse(&id)).transpose()?,
                    reason: QuarantineReason::from_code(&reason)?,
                })
            })
            .collect()
    }

    /// Ledgered commands for this gateway, active epoch, with no attempt yet.
    pub fn pending_commands(&self) -> Result<Vec<ReceivedCommand>, Error> {
        let s = self.lock()?;
        let Some(active) = s.active_epoch else {
            return Ok(Vec::new());
        };
        if restore_guarded(&s.conn)? {
            return Ok(Vec::new());
        }
        let mut query = s.conn.prepare(
            "SELECT l.command_id,l.subscription_id,l.payload FROM command_ledger l LEFT JOIN attempts a ON a.command_id=l.command_id WHERE a.command_id IS NULL AND l.historical=0 AND l.key_epoch=? ORDER BY l.local_order",
        )?;
        let rows = query
            .query_map(params![active], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, subscription_id, payload)| {
                Ok(ReceivedCommand {
                    command_id: parse(&id)?,
                    subscription_id,
                    message: localize(&s.conn, decode(payload.as_bytes())?)?.0,
                })
            })
            .collect()
    }

    /// Durably records an attempt before the native host touches the carrier API.
    pub fn begin_send_attempt(&self, command_id: CommandId) -> Result<PermitDecision, Error> {
        let mut guard = self.lock()?;
        let active = guard.active_epoch;
        let tx = guard
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ledger: Option<(u32, String, String, bool)> = tx
            .query_row(
                "SELECT key_epoch,subscription_id,payload,historical FROM command_ledger WHERE command_id=?",
                params![command_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((epoch, subscription_id, payload, historical)) = ledger else {
            return Ok(PermitDecision::Blocked(PermitBlock::NotReceived));
        };
        // Known attempts keep reporting their recorded (or unknown) outcome.
        if let Some(state) = attempt_state(&tx, command_id)? {
            return Ok(PermitDecision::AlreadyAttempted(state));
        }
        if restore_guarded(&tx)? {
            return Ok(PermitDecision::Blocked(PermitBlock::RestoreGuarded));
        }
        if historical {
            return Ok(PermitDecision::Blocked(PermitBlock::Historical));
        }
        match active {
            Some(active) if epoch == active => {}
            Some(active) if epoch < active => {
                return Ok(PermitDecision::Blocked(PermitBlock::StaleEpoch));
            }
            _ => return Ok(PermitDecision::Blocked(PermitBlock::EpochNotActive)),
        }
        let (message, media_ready) = localize(&tx, decode(payload.as_bytes())?)?;
        if !media_ready {
            return Ok(PermitDecision::Blocked(PermitBlock::MediaUnavailable));
        }
        tx.execute(
            "INSERT INTO attempts(command_id,state) VALUES(?,?)",
            params![
                command_id.to_string(),
                state_code(SendState::AttemptRecorded)
            ],
        )?;
        tx.commit()?;
        Ok(PermitDecision::Permit(ReceivedCommand {
            command_id,
            subscription_id,
            message,
        }))
    }

    /// Records native carrier evidence and queues an encrypted `SendStatus`. Repeating the
    /// current state is a no-op.
    pub fn record_send_result(
        &self,
        command_id: CommandId,
        result: SendResult,
    ) -> Result<SendState, Error> {
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = attempt_state(&tx, command_id)?.ok_or(Error::NoAttempt)?;
        let next = SendState::from(result);
        if current == next {
            return Ok(next);
        }
        if !legal_transition(current, next) {
            return Err(Error::IllegalTransition {
                from: current,
                to: next,
            });
        }
        tx.execute(
            "UPDATE attempts SET state=? WHERE command_id=?",
            params![state_code(next), command_id.to_string()],
        )?;
        emit_status(&tx, &ctx, command_id, next)?;
        tx.commit()?;
        Ok(next)
    }

    /// Conversations ordered by most recent local arrival.
    pub fn list_conversations(&self) -> Result<Vec<Conversation>, Error> {
        let s = self.lock()?;
        let mut query = s.conn.prepare(
            "SELECT conversation_id,SUM(seen=0) FROM messages GROUP BY conversation_id ORDER BY MAX(local_order) DESC",
        )?;
        let rows = query
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, unread)| {
                Ok(Conversation {
                    conversation_id: parse(&id)?,
                    unread_count: u64::try_from(unread).map_err(|_| Error::Database)?,
                })
            })
            .collect()
    }

    pub fn messages(&self, conversation_id: ConversationId) -> Result<Vec<Message>, Error> {
        let s = self.lock()?;
        let mut query = s.conn.prepare(
            "SELECT m.payload, m.seen,
                (SELECT st.state FROM commands c JOIN send_status st ON st.command_id=c.command_id AND st.producer_device_id=c.gateway_device_id WHERE c.message_id=m.id),
                (SELECT o.state FROM commands c JOIN outbox o ON o.command_id=c.command_id WHERE c.message_id=m.id),
                EXISTS(SELECT 1 FROM commands c WHERE c.message_id=m.id)
             FROM messages m WHERE m.conversation_id=? ORDER BY m.local_order",
        )?;
        let rows = query
            .query_map(params![conversation_id.to_string()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, bool>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(payload, seen, status, outbox, is_command)| {
                let send_state = if !is_command {
                    None
                } else if let Some(status) = status {
                    Some(decode_state(&status)?)
                } else {
                    Some(match outbox.as_deref() {
                        Some("unsealed" | "queued") => SendState::QueuedLocal,
                        _ => SendState::AcceptedServer,
                    })
                };
                Ok(Message {
                    payload: localize(&s.conn, decode(payload.as_bytes())?)?.0,
                    seen,
                    send_state,
                })
            })
            .collect()
    }

    /// Marks one displayed incoming message seen and queues an encrypted `ReadState`.
    /// Returns false if it was already seen (or is outgoing).
    pub fn mark_seen(&self, message_id: MessageId) -> Result<bool, Error> {
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seen: bool = tx
            .query_row(
                "SELECT seen FROM messages WHERE id=?",
                params![message_id.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::NotFound)?;
        if seen {
            return Ok(false);
        }
        record_seen(&tx, message_id)?;
        enqueue(
            &tx,
            &ctx,
            EnvelopePurpose::Event,
            None,
            None,
            &PrivatePayload::ReadState { message_id },
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn unread_count(&self, conversation_id: ConversationId) -> Result<u64, Error> {
        let count: i64 = self.lock()?.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id=? AND seen=0",
            params![conversation_id.to_string()],
            |r| r.get(0),
        )?;
        u64::try_from(count).map_err(|_| Error::Database)
    }

    /// Compare-and-swap draft save; a stale revision leaves the stored content untouched.
    pub fn save_draft(
        &self,
        conversation_id: ConversationId,
        content: &str,
        expected_revision: u64,
    ) -> Result<Draft, Error> {
        let s = self.lock()?;
        let current: i64 = s
            .conn
            .query_row(
                "SELECT revision FROM drafts WHERE conversation_id=?",
                params![conversation_id.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let current = u64::try_from(current).map_err(|_| Error::Database)?;
        if current != expected_revision {
            return Err(Error::StaleDraft {
                current_revision: current,
            });
        }
        let next = current + 1;
        s.conn.execute(
            "INSERT INTO drafts(conversation_id,content,revision) VALUES(?,?,?) ON CONFLICT(conversation_id) DO UPDATE SET content=excluded.content, revision=excluded.revision",
            params![conversation_id.to_string(), content, to_i64(next)?],
        )?;
        Ok(Draft {
            conversation_id,
            content: content.into(),
            revision: next,
        })
    }

    pub fn draft(&self, conversation_id: ConversationId) -> Result<Option<Draft>, Error> {
        let s = self.lock()?;
        let row: Option<(String, i64)> = s
            .conn
            .query_row(
                "SELECT content,revision FROM drafts WHERE conversation_id=?",
                params![conversation_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(content, revision)| {
            Ok(Draft {
                conversation_id,
                content,
                revision: u64::try_from(revision).map_err(|_| Error::Database)?,
            })
        })
        .transpose()
    }

    /// Encrypts an app-owned regular file (<= 32 MiB) with a fresh file key into the core media
    /// store. Runs outside the database lock. `display_name` is sanitized and never used in paths.
    pub fn prepare_attachment(
        &self,
        source: &Path,
        media_type: &str,
        display_name: &str,
    ) -> Result<AttachmentInfo, Error> {
        if !media::valid_media_type(media_type) {
            return Err(Error::InvalidRequest("media type"));
        }
        let display_name = media::sanitize_display_name(display_name);
        let (root, vault_id) = {
            let s = self.lock()?;
            (s.media_dir.clone(), s.config.vault_id)
        };
        let attachment_id = AttachmentId::new();
        let key = FileKey::generate().map_err(|_| Error::Crypto)?;
        let aad = media::media_aad(vault_id, attachment_id);
        let encrypted = media::encrypt_into_store(&root, attachment_id, source, &key, &aad)?;
        let key_bytes = Zeroizing::new(key.with_encrypted_reference_bytes(|bytes| *bytes));
        let inserted = self.lock().and_then(|s| {
            s.conn
                .execute(
                    "INSERT INTO attachments(attachment_id,state,media_type,display_name,plaintext_bytes,ciphertext_bytes,ciphertext_sha256,stream_version,file_key,remote_object_id) VALUES(?,?,?,?,?,?,?,?,?,NULL)",
                    params![
                        attachment_id.to_string(),
                        AttachmentState::PendingUpload.code(),
                        media_type,
                        display_name,
                        to_i64(encrypted.plaintext_bytes)?,
                        to_i64(encrypted.ciphertext_bytes)?,
                        encrypted.ciphertext_sha256,
                        STREAM_VERSION,
                        key_bytes.as_slice()
                    ],
                )
                .map_err(Error::from)
        });
        if let Err(error) = inserted {
            let _ = std::fs::remove_file(media::cipher_path(&root, attachment_id));
            return Err(error);
        }
        Ok(AttachmentInfo {
            attachment_id,
            media_type: media_type.to_owned(),
            display_name,
            plaintext_bytes: encrypted.plaintext_bytes,
            ciphertext_bytes: encrypted.ciphertext_bytes,
            ciphertext_sha256: encrypted.ciphertext_sha256,
            state: AttachmentState::PendingUpload,
        })
    }

    /// Sanitized metadata (no key, no path) for UI rendering.
    pub fn attachment_info(&self, attachment_id: AttachmentId) -> Result<AttachmentInfo, Error> {
        let s = self.lock()?;
        Ok(attachment_row(&s.conn, attachment_id)?
            .ok_or(Error::NotFound)?
            .info(attachment_id))
    }

    /// Locally prepared objects referenced by a message and not yet uploaded.
    pub fn pending_uploads(&self) -> Result<Vec<CipherObject>, Error> {
        self.cipher_objects(
            "SELECT DISTINCT a.attachment_id FROM attachments a JOIN message_attachments m ON m.attachment_id=a.attachment_id WHERE a.state='pending_upload' ORDER BY a.rowid",
        )
    }

    /// Received objects whose ciphertext must be downloaded and installed.
    pub fn pending_downloads(&self) -> Result<Vec<CipherObject>, Error> {
        self.cipher_objects(
            "SELECT attachment_id FROM attachments WHERE state='pending_download' ORDER BY rowid",
        )
    }

    /// Native-only path to verified local ciphertext (for upload). Never hand it to a web view.
    pub fn native_cipher_file(&self, attachment_id: AttachmentId) -> Result<PathBuf, Error> {
        let s = self.lock()?;
        let row = attachment_row(&s.conn, attachment_id)?.ok_or(Error::NotFound)?;
        if !row.state.is_local() {
            return Err(Error::InvalidMedia);
        }
        Ok(media::cipher_path(&s.media_dir, attachment_id))
    }

    /// Records the server object ID returned by reserve/upload/finalize, then seals any held
    /// MMS work whose media is now complete. Idempotent for the same ID.
    pub fn mark_attachment_uploaded(
        &self,
        attachment_id: AttachmentId,
        remote_object_id: &str,
    ) -> Result<(), Error> {
        let remote = uuid::Uuid::parse_str(remote_object_id)
            .map_err(|_| Error::InvalidRequest("remote object id"))?
            .to_string();
        let mut guard = self.lock()?;
        let row = attachment_row(&guard.conn, attachment_id)?.ok_or(Error::NotFound)?;
        match (row.state, row.remote_object_id) {
            (AttachmentState::PendingUpload, _) => {
                guard.conn.execute(
                    "UPDATE attachments SET state=?,remote_object_id=? WHERE attachment_id=?",
                    params![
                        AttachmentState::Uploaded.code(),
                        remote,
                        attachment_id.to_string()
                    ],
                )?;
            }
            (_, Some(existing)) if existing == remote => {}
            _ => return Err(Error::Conflict),
        }
        seal_with_store(&mut guard)
    }

    /// Verifies a natively downloaded ciphertext file (exact length, SHA-256, full secretstream
    /// decryption with final tag and no trailing data) and installs a private copy. On failure the
    /// attachment stays `PendingDownload` and nothing is promoted. The input file is not removed.
    pub fn install_downloaded_attachment(
        &self,
        attachment_id: AttachmentId,
        downloaded: &Path,
    ) -> Result<(), Error> {
        let (root, vault_id, row) = {
            let s = self.lock()?;
            let row = attachment_row(&s.conn, attachment_id)?.ok_or(Error::NotFound)?;
            (s.media_dir.clone(), s.config.vault_id, row)
        };
        if row.state != AttachmentState::PendingDownload {
            return Ok(());
        }
        media::install_into_store(
            &root,
            attachment_id,
            downloaded,
            &media::Expected {
                ciphertext_bytes: row.ciphertext_bytes,
                ciphertext_sha256: &row.ciphertext_sha256,
                plaintext_bytes: row.plaintext_bytes,
            },
            &row.file_key()?,
            &media::media_aad(vault_id, attachment_id),
        )?;
        self.lock()?.conn.execute(
            "UPDATE attachments SET state=? WHERE attachment_id=? AND state=?",
            params![
                AttachmentState::Available.code(),
                attachment_id.to_string(),
                AttachmentState::PendingDownload.code()
            ],
        )?;
        Ok(())
    }

    /// Decrypts a locally verified attachment to a native-only temporary plaintext file for
    /// preview or MMS PDU encoding. The file is removed when the handle drops (and purged on the
    /// next open after a crash).
    pub fn open_native_plaintext(
        &self,
        attachment_id: AttachmentId,
    ) -> Result<NativePlaintextFile, Error> {
        let (root, vault_id, row) = {
            let s = self.lock()?;
            let row = attachment_row(&s.conn, attachment_id)?.ok_or(Error::NotFound)?;
            (s.media_dir.clone(), s.config.vault_id, row)
        };
        if !row.state.is_local() {
            return Err(Error::InvalidMedia);
        }
        media::open_plaintext(
            &root,
            attachment_id,
            &row.file_key()?,
            &media::media_aad(vault_id, attachment_id),
            row.plaintext_bytes,
            self.0.clone(),
        )
    }

    /// Seals up to `limit` locked/held outbox rows (implicit sealing is capped at
    /// `MAX_SEAL_BATCH`). Returns how many were sealed; call until it returns 0.
    pub fn seal_pending_batch(&self, limit: usize) -> Result<usize, Error> {
        let mut guard = self.lock()?;
        seal_store_batch(&mut guard, limit)
    }

    /// Starts a staged snapshot generation pinned to the server's fixed `high_water` cursor and
    /// record count (`0/0` is a valid empty snapshot). Any older unpublished staging becomes
    /// obsolete. `Restore` durably sets the restore guard first, even if the arguments or later
    /// pages are rejected. Refused while a previously published snapshot is still draining.
    pub fn begin_snapshot(
        &self,
        high_water: Cursor,
        record_count: u64,
        purpose: SnapshotPurpose,
    ) -> Result<SnapshotProgress, Error> {
        let mut guard = self.lock()?;
        if purpose == SnapshotPurpose::Restore {
            set_meta(&guard.conn, "restore_guard", "1")?;
        }
        let high = i64::try_from(high_water.0).map_err(|_| Error::InvalidCursor)?;
        if record_count > MAX_SNAPSHOT_RECORDS || record_count > high_water.0 {
            return Err(Error::InvalidRequest("snapshot record count"));
        }
        let tx = guard
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if active_generation(&tx)?.is_some_and(|g| g.published) {
            return Err(Error::InvalidRequest(
                "previous snapshot is still draining; call apply_pending",
            ));
        }
        tx.execute(
            "UPDATE snapshot_generations SET state='obsolete' WHERE state='staging'",
            [],
        )?;
        let generation = get_meta(&tx, "snapshot_generation")?
            .map(|g| g.parse::<u64>().map_err(|_| Error::Database))
            .transpose()?
            .unwrap_or(0)
            + 1;
        set_meta(&tx, "snapshot_generation", &generation.to_string())?;
        tx.execute(
            "INSERT INTO snapshot_generations(generation,high_water,expected,received,received_bytes,overlap,last_cursor,state) VALUES(?,?,?,0,0,0,0,'staging')",
            params![to_i64(generation)?, high, to_i64(record_count)?],
        )?;
        collect_snapshot_garbage(&tx)?;
        tx.commit()?;
        Ok(SnapshotProgress {
            generation,
            high_water,
            expected_records: record_count,
            received_records: 0,
            last_cursor: Cursor(0),
        })
    }

    /// The current unpublished generation, if any (survives restarts so native code can resume
    /// from `last_cursor` or begin again).
    pub fn snapshot_progress(&self) -> Result<Option<SnapshotProgress>, Error> {
        let s = self.lock()?;
        active_generation(&s.conn)?
            .filter(|g| !g.published)
            .map(|g| g.progress())
            .transpose()
    }

    /// Stages up to `MAX_SNAPSHOT_PAGE` typed records (`MAX_SNAPSHOT_PAGE_BYTES` of canonical
    /// JSON). See `append_snapshot_raw_page` for the validation rules.
    pub fn append_snapshot_page(
        &self,
        generation: u64,
        records: &[SnapshotRecord],
    ) -> Result<SnapshotProgress, Error> {
        self.check_page_shape(records.len())?;
        let mut page = Vec::with_capacity(records.len());
        let mut input_bytes = 0usize;
        for record in records {
            let wire = typed_wire(&record.envelope)?;
            input_bytes = input_bytes.saturating_add(wire.len());
            if input_bytes > MAX_SNAPSHOT_PAGE_BYTES {
                return Err(Error::InvalidRequest("snapshot page bytes"));
            }
            page.push((record.cursor, wire));
        }
        self.append_wires(generation, page)
    }

    /// Stages up to `MAX_SNAPSHOT_PAGE` raw server records totalling at most
    /// `MAX_SNAPSHOT_PAGE_BYTES`. Each record is canonicalized exactly like `ingest_raw`
    /// (oversize -> digest marker, unparseable kept and quarantined when drained). Cursors must
    /// strictly increase across pages and stay <= high-water; the generation's count and total
    /// stored bytes are capped; a record differing from an already journaled cursor fails the
    /// generation. Rejected generations never affect live state or a newer generation.
    pub fn append_snapshot_raw_page(
        &self,
        generation: u64,
        records: &[RawSnapshotRecord],
    ) -> Result<SnapshotProgress, Error> {
        self.check_page_shape(records.len())?;
        let input_bytes = records
            .iter()
            .fold(0usize, |sum, r| sum.saturating_add(r.envelope_json.len()));
        if input_bytes > MAX_SNAPSHOT_PAGE_BYTES {
            return Err(Error::InvalidRequest("snapshot page bytes"));
        }
        let page = records
            .iter()
            .map(|r| Ok((r.cursor, raw_wire(&r.envelope_json)?)))
            .collect::<Result<Vec<_>, Error>>()?;
        self.append_wires(generation, page)
    }

    /// Publishes a complete generation in one short transaction: marks it published and raises
    /// the receive cursor to its high-water. No records are copied here; `apply_pending` drains
    /// them in bounded batches (commands become historical: never permitted, no receipts).
    /// Stale or incomplete generations return `SnapshotMismatch` without touching a newer one.
    pub fn finish_snapshot(&self, generation: u64) -> Result<SnapshotReport, Error> {
        let mut guard = self.lock()?;
        let tx = guard
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = current_staging(&tx, generation)?.ok_or(Error::SnapshotMismatch)?;
        if current.received != current.expected {
            tx.execute(
                "UPDATE snapshot_generations SET state='failed' WHERE generation=?",
                params![current.number],
            )?;
            tx.commit()?;
            return Err(Error::SnapshotMismatch);
        }
        let receive_cursor = advance_frontier(
            &tx,
            u64::try_from(current.high_water).map_err(|_| Error::Database)?,
        )?;
        if current.expected == 0 {
            tx.execute(
                "DELETE FROM snapshot_generations WHERE generation=?",
                params![current.number],
            )?;
        } else {
            tx.execute(
                "UPDATE snapshot_generations SET state='published' WHERE generation=?",
                params![current.number],
            )?;
        }
        tx.commit()?;
        let count = |v: i64| usize::try_from(v).map_err(|_| Error::Database);
        Ok(SnapshotReport {
            journaled: count(current.expected - current.overlap)?,
            duplicate: count(current.overlap)?,
            quarantined: 0,
            receive_cursor,
        })
    }

    /// Local outbox rows whose envelope IDs the server holds with different bytes. They are
    /// excluded from `pending_outbox`/sealing and must be reconciled, never auto-resent.
    pub fn outbox_conflicts(&self) -> Result<Vec<OutboxConflict>, Error> {
        let s = self.lock()?;
        let rows: Vec<(String, Option<String>, i64)> = s
            .conn
            .prepare(
                "SELECT c.envelope_id,o.command_id,c.cursor FROM outbox_conflicts c JOIN outbox o ON o.envelope_id=c.envelope_id ORDER BY o.seq",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|(envelope_id, command_id, cursor)| {
                Ok(OutboxConflict {
                    envelope_id: parse(&envelope_id)?,
                    command_id: command_id.map(|id| parse(&id)).transpose()?,
                    cursor: Cursor(u64::try_from(cursor).map_err(|_| Error::Database)?),
                })
            })
            .collect()
    }

    fn check_page_shape(&self, records: usize) -> Result<(), Error> {
        if records == 0 || records > MAX_SNAPSHOT_PAGE {
            return Err(Error::InvalidRequest("snapshot page size"));
        }
        Ok(())
    }

    fn append_wires(
        &self,
        generation: u64,
        page: Vec<(Cursor, Vec<u8>)>,
    ) -> Result<SnapshotProgress, Error> {
        let mut guard = self.lock()?;
        let tx = guard
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut current = current_staging(&tx, generation)?.ok_or(Error::SnapshotMismatch)?;
        let page_bytes: i64 = page.iter().map(|(_, wire)| wire.len() as i64).sum();
        // Validate the whole page with indexed lookups before staging anything.
        let mut last = current.last_cursor;
        let mut overlap = 0;
        let mut valid = current.received + page.len() as i64 <= current.expected
            && current.received_bytes + page_bytes <= MAX_SNAPSHOT_BYTES as i64;
        for (cursor, wire) in &page {
            let cursor = i64::try_from(cursor.0).unwrap_or(i64::MAX);
            if !valid || cursor <= last || cursor > current.high_water {
                valid = false;
                break;
            }
            last = cursor;
            let journaled: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT wire FROM journal WHERE cursor=?",
                    params![cursor],
                    |r| r.get(0),
                )
                .optional()?;
            match journaled {
                Some(existing) if existing != *wire => valid = false, // hard committed conflict
                Some(_) => overlap += 1,
                None => {}
            }
        }
        if !valid {
            tx.execute(
                "UPDATE snapshot_generations SET state='failed' WHERE generation=?",
                params![current.number],
            )?;
            tx.commit()?;
            return Err(Error::SnapshotMismatch);
        }
        for (cursor, wire) in &page {
            tx.execute(
                "INSERT INTO snapshot_records(generation,cursor,wire) VALUES(?,?,?)",
                params![current.number, to_i64(cursor.0)?, wire],
            )?;
        }
        current.received += page.len() as i64;
        current.received_bytes += page_bytes;
        current.overlap += overlap;
        current.last_cursor = last;
        tx.execute(
            "UPDATE snapshot_generations SET received=?,received_bytes=?,overlap=?,last_cursor=? WHERE generation=?",
            params![
                current.received,
                current.received_bytes,
                current.overlap,
                current.last_cursor,
                current.number
            ],
        )?;
        tx.commit()?;
        current.progress()
    }

    /// Durably marks this device as a restored gateway: no new carrier permits, ever. Known
    /// attempts still report their state. Recovery is re-enrollment with a fresh device
    /// identity and database plus explicit reconciliation; there is intentionally no clear.
    pub fn mark_restored_gateway(&self) -> Result<(), Error> {
        set_meta(&self.lock()?.conn, "restore_guard", "1")
    }

    pub fn restore_guarded(&self) -> Result<bool, Error> {
        restore_guarded(&self.lock()?.conn)
    }

    /// Ledgered commands this gateway must reconcile instead of executing: historical
    /// (snapshot), guard-blocked, or attempted with an unknown outcome.
    pub fn commands_needing_reconciliation(&self) -> Result<Vec<ReconciliationItem>, Error> {
        let s = self.lock()?;
        let guarded = restore_guarded(&s.conn)?;
        let rows: Vec<(String, String, bool, Option<String>)> = s
            .conn
            .prepare(
                "SELECT l.command_id,l.payload,l.historical,a.state FROM command_ledger l LEFT JOIN attempts a ON a.command_id=l.command_id WHERE l.retired=0 ORDER BY l.local_order",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
        let mut items = Vec::new();
        for (id, payload, historical, attempt) in rows {
            let reason = match attempt.as_deref().map(decode_state).transpose()? {
                Some(SendState::OutcomeUnknown) => ReconciliationReason::OutcomeUnknown,
                Some(_) => continue,
                None if historical => ReconciliationReason::Historical,
                None if guarded => ReconciliationReason::RestoreGuarded,
                None => continue,
            };
            items.push(ReconciliationItem {
                command_id: parse(&id)?,
                message: localize(&s.conn, decode(payload.as_bytes())?)?.0,
                reason,
            });
        }
        Ok(items)
    }

    fn cipher_objects(&self, sql: &str) -> Result<Vec<CipherObject>, Error> {
        let s = self.lock()?;
        let ids: Vec<String> = s
            .conn
            .prepare(sql)?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        ids.into_iter()
            .map(|id| {
                let attachment_id = parse(&id)?;
                let row = attachment_row(&s.conn, attachment_id)?.ok_or(Error::Database)?;
                Ok(CipherObject {
                    attachment_id,
                    ciphertext_bytes: row.ciphertext_bytes,
                    ciphertext_sha256: row.ciphertext_sha256,
                    remote_object_id: row.remote_object_id,
                })
            })
            .collect()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Store>, Error> {
        self.0.lock().map_err(|_| Error::Database)
    }

    fn journal(
        &self,
        cursor: Cursor,
        wire: Vec<u8>,
        envelope: Option<&Envelope>,
    ) -> Result<IngestResult, Error> {
        let cursor = i64::try_from(cursor.0)
            .ok()
            .filter(|cursor| *cursor > 0)
            .ok_or(Error::InvalidCursor)?;
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT wire FROM journal WHERE cursor=?",
                params![cursor],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            return if existing == wire {
                Ok(IngestResult::Duplicate)
            } else {
                Err(Error::Conflict)
            };
        }
        // Coordinate with the active snapshot generation at this cursor.
        if let Some(generation) = active_generation(&tx)? {
            let staged: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT wire FROM snapshot_records WHERE generation=? AND cursor=?",
                    params![generation.number, cursor],
                    |r| r.get(0),
                )
                .optional()?;
            match (staged, generation.published) {
                // Published records are committed history awaiting drain.
                (Some(staged), true) => {
                    return if staged == wire {
                        Ok(IngestResult::Duplicate)
                    } else {
                        Err(Error::Conflict)
                    };
                }
                // Live data wins over unpublished staging; that staging can no longer publish.
                (Some(staged), false) if staged != wire => {
                    tx.execute(
                        "UPDATE snapshot_generations SET state='failed' WHERE generation=?",
                        params![generation.number],
                    )?;
                }
                _ => {}
            }
        }
        let result = classify(&tx, &ctx, cursor, &wire, envelope)?;
        let (status, reason) = match result {
            IngestResult::Journaled => ("pending", None),
            IngestResult::Duplicate => ("duplicate", None),
            IngestResult::Quarantined(reason) => ("quarantined", Some(reason.code())),
        };
        tx.execute(
            "INSERT INTO journal(cursor,envelope_id,key_epoch,wire,status,reason) VALUES(?,?,?,?,?,?)",
            params![
                cursor,
                envelope.map(|e| e.envelope_id.to_string()),
                envelope.map(|e| e.key_epoch),
                wire,
                status,
                reason
            ],
        )?;
        advance_frontier(&tx, 0)?;
        tx.commit()?;
        Ok(result)
    }
}

/// Raises the receive cursor to at least `floor`, then over any contiguous journaled cursors.
fn advance_frontier(conn: &Connection, floor: u64) -> Result<u64, Error> {
    let start = receive_cursor(conn)?;
    let mut frontier = start.max(floor);
    while conn
        .query_row(
            "SELECT 1 FROM journal WHERE cursor=?",
            params![to_i64(frontier + 1)?],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        frontier += 1;
    }
    if frontier != start {
        set_meta(conn, "receive_cursor", &frontier.to_string())?;
    }
    Ok(frontier)
}

fn restore_guarded(conn: &Connection) -> Result<bool, Error> {
    Ok(get_meta(conn, "restore_guard")?.is_some())
}

/// The single staging or published generation (begin refuses to overlap them).
struct Generation {
    number: i64,
    high_water: i64,
    expected: i64,
    received: i64,
    received_bytes: i64,
    overlap: i64,
    last_cursor: i64,
    drained: i64,
    published: bool,
}
impl Generation {
    fn progress(&self) -> Result<SnapshotProgress, Error> {
        let u = |v: i64| u64::try_from(v).map_err(|_| Error::Database);
        Ok(SnapshotProgress {
            generation: u(self.number)?,
            high_water: Cursor(u(self.high_water)?),
            expected_records: u(self.expected)?,
            received_records: u(self.received)?,
            last_cursor: Cursor(u(self.last_cursor)?),
        })
    }
}
fn active_generation(conn: &Connection) -> Result<Option<Generation>, Error> {
    Ok(conn
        .query_row(
            "SELECT generation,high_water,expected,received,received_bytes,overlap,last_cursor,state,drained FROM snapshot_generations WHERE state IN ('staging','published') ORDER BY generation DESC LIMIT 1",
            [],
            |r| {
                Ok(Generation {
                    number: r.get(0)?,
                    high_water: r.get(1)?,
                    expected: r.get(2)?,
                    received: r.get(3)?,
                    received_bytes: r.get(4)?,
                    overlap: r.get(5)?,
                    last_cursor: r.get(6)?,
                    published: r.get::<_, String>(7)? == "published",
                    drained: r.get(8)?,
                })
            },
        )
        .optional()?)
}
/// The caller's generation only if it is the newest and still staging.
fn current_staging(conn: &Connection, generation: u64) -> Result<Option<Generation>, Error> {
    Ok(active_generation(conn)?
        .filter(|g| !g.published && u64::try_from(g.number).ok() == Some(generation)))
}
/// Moves up to `limit` published records into the journal (classified like live ingest, marked
/// historical; own-echo acknowledgments happen only here, after publication). Returns the last
/// drained cursor while records remain, or `None` when no snapshot is draining.
fn drain_snapshot(
    conn: &Connection,
    ctx: &Ctx,
    limit: usize,
    report: &mut ApplyReport,
) -> Result<Option<i64>, Error> {
    let Some(generation) = active_generation(conn)?.filter(|g| g.published) else {
        return Ok(None);
    };
    let mut query = conn.prepare(
        "SELECT LENGTH(wire),cursor,wire FROM snapshot_records WHERE generation=? ORDER BY cursor LIMIT ?",
    )?;
    let rows: Vec<(i64, Vec<u8>)> = take_within_budget(
        query.query(params![generation.number, to_i64(limit as u64)?])?,
        MAX_SNAPSHOT_PAGE_BYTES,
        |r| Ok((r.get(1)?, r.get(2)?)),
    )?;
    let mut drained = 0i64;
    for (cursor, wire) in rows {
        let journaled = conn
            .query_row(
                "SELECT 1 FROM journal WHERE cursor=?",
                params![cursor],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        // A journaled cursor was verified equal at append, and live ingest never inserts a
        // different record over published staging.
        if !journaled {
            let envelope = parse_wire(&wire);
            let (status, reason) = match classify(conn, ctx, cursor, &wire, envelope.as_ref())? {
                IngestResult::Journaled => ("pending", None),
                IngestResult::Duplicate => ("duplicate", None),
                IngestResult::Quarantined(reason) => ("quarantined", Some(reason.code())),
            };
            conn.execute(
                "INSERT INTO journal(cursor,envelope_id,key_epoch,wire,status,reason,historical) VALUES(?,?,?,?,?,?,1)",
                params![
                    cursor,
                    envelope.as_ref().map(|e| e.envelope_id.to_string()),
                    envelope.as_ref().map(|e| e.key_epoch),
                    wire,
                    status,
                    reason
                ],
            )?;
        }
        conn.execute(
            "DELETE FROM snapshot_records WHERE generation=? AND cursor=?",
            params![generation.number, cursor],
        )?;
        drained += 1;
        report.drained += 1;
    }
    conn.execute(
        "UPDATE snapshot_generations SET drained=drained+? WHERE generation=?",
        params![drained, generation.number],
    )?;
    report.snapshot_remaining = u64::try_from(generation.received - generation.drained - drained)
        .map_err(|_| Error::Database)?;
    let next: Option<i64> = conn.query_row(
        "SELECT MIN(cursor) FROM snapshot_records WHERE generation=?",
        params![generation.number],
        |r| r.get(0),
    )?;
    match next {
        None => {
            conn.execute(
                "DELETE FROM snapshot_generations WHERE generation=?",
                params![generation.number],
            )?;
            Ok(None)
        }
        // Only cursors before the next undrained record may apply (live tails wait for history).
        Some(next) => Ok(Some(next - 1)),
    }
}
/// Reads an ordered prefix of `rows` whose first column is `LENGTH(wire)`, stopping before the
/// row that would push the total past `budget` bytes. The length is checked before `read`
/// allocates the row, and the first row is always taken (every stored wire is itself bounded),
/// so one batch holds at most `budget` bytes, or one record if that record alone is larger.
fn take_within_budget<T>(
    mut rows: rusqlite::Rows<'_>,
    budget: usize,
    mut read: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, Error> {
    let mut taken = Vec::new();
    let mut used = 0usize;
    while let Some(row) = rows.next()? {
        let bytes = usize::try_from(row.get::<_, i64>(0)?).map_err(|_| Error::Database)?;
        if !taken.is_empty() && used.saturating_add(bytes) > budget {
            break;
        }
        used = used.saturating_add(bytes);
        taken.push(read(row)?);
    }
    Ok(taken)
}

/// Removes a bounded batch of obsolete/failed staging rows and empty generation rows.
fn collect_snapshot_garbage(conn: &Connection) -> Result<(), Error> {
    conn.execute(
        "DELETE FROM snapshot_records WHERE rowid IN (SELECT r.rowid FROM snapshot_records r JOIN snapshot_generations g ON g.generation=r.generation WHERE g.state IN ('failed','obsolete') LIMIT ?)",
        params![SNAPSHOT_GC_BATCH],
    )?;
    conn.execute(
        "DELETE FROM snapshot_generations WHERE state IN ('failed','obsolete') AND NOT EXISTS (SELECT 1 FROM snapshot_records r WHERE r.generation=snapshot_generations.generation)",
        [],
    )?;
    Ok(())
}
/// Canonical stored bytes for typed input (oversize serialization becomes a digest marker).
fn typed_wire(envelope: &Envelope) -> Result<Vec<u8>, Error> {
    let json = serde_json::to_vec(envelope).map_err(|_| Error::Database)?;
    raw_wire(&json)
}
/// Canonical stored bytes for raw server JSON, shared by live ingest and snapshot import:
/// oversize input -> `oversize:<len>:<sha256>` marker, parseable -> canonical re-serialization,
/// otherwise the raw bytes (quarantined as malformed).
fn raw_wire(json: &[u8]) -> Result<Vec<u8>, Error> {
    if json.len() > MAX_RAW_ENVELOPE_BYTES {
        let mut marker = format!("oversize:{}:", json.len()).into_bytes();
        marker.extend_from_slice(&Sha256::digest(json));
        return Ok(marker);
    }
    match serde_json::from_slice::<Envelope>(json) {
        Ok(envelope) => serde_json::to_vec(&envelope).map_err(|_| Error::Database),
        Err(_) => Ok(json.to_vec()),
    }
}
fn parse_wire(wire: &[u8]) -> Option<Envelope> {
    serde_json::from_slice(wire).ok()
}

const SCHEMA: &str = "
CREATE TABLE metadata(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE key_epoch_profiles(epoch INTEGER PRIMARY KEY, profile TEXT NOT NULL);
CREATE TABLE journal(cursor INTEGER PRIMARY KEY, envelope_id TEXT, key_epoch INTEGER, wire BLOB NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('pending','applied','duplicate','quarantined')), reason TEXT);
CREATE INDEX journal_envelope ON journal(envelope_id);
CREATE INDEX journal_pending ON journal(status, key_epoch, cursor);
CREATE TABLE outbox(seq INTEGER PRIMARY KEY, envelope_id TEXT NOT NULL UNIQUE, command_id TEXT UNIQUE,
  purpose TEXT NOT NULL CHECK(purpose IN ('command','event')), route TEXT, plain BLOB, wire BLOB,
  state TEXT NOT NULL CHECK(state IN ('unsealed','queued','acknowledged')));
CREATE TABLE messages(local_order INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
  conversation_id TEXT NOT NULL, source_device_id TEXT NOT NULL, source_sequence INTEGER NOT NULL,
  provider_message_id TEXT, payload TEXT NOT NULL, seen INTEGER NOT NULL,
  UNIQUE(conversation_id, source_device_id, source_sequence));
CREATE UNIQUE INDEX messages_provider ON messages(source_device_id, provider_message_id) WHERE provider_message_id IS NOT NULL;
CREATE INDEX messages_conversation ON messages(conversation_id, local_order);
CREATE TABLE addresses(address TEXT PRIMARY KEY, conversation_id TEXT NOT NULL);
CREATE TABLE seen_messages(id TEXT PRIMARY KEY);
CREATE TABLE commands(command_id TEXT PRIMARY KEY, message_id TEXT NOT NULL, gateway_device_id TEXT NOT NULL);
CREATE INDEX commands_message ON commands(message_id);
CREATE TABLE send_status(command_id TEXT NOT NULL, producer_device_id TEXT NOT NULL,
  producer_sequence INTEGER NOT NULL, state TEXT NOT NULL, PRIMARY KEY(command_id, producer_device_id));
CREATE TABLE command_ledger(local_order INTEGER PRIMARY KEY AUTOINCREMENT, command_id TEXT NOT NULL UNIQUE,
  key_epoch INTEGER NOT NULL, subscription_id TEXT NOT NULL, payload TEXT NOT NULL);
CREATE TABLE attempts(command_id TEXT PRIMARY KEY REFERENCES command_ledger(command_id), state TEXT NOT NULL);
CREATE TABLE drafts(conversation_id TEXT PRIMARY KEY, content TEXT NOT NULL, revision INTEGER NOT NULL);
";

/// Version 4 -> 5: cutover retirement marker, native key-cache checks, compose drafts.
const MIGRATION_5: &str = "
ALTER TABLE command_ledger ADD COLUMN retired INTEGER NOT NULL DEFAULT 0;
CREATE TABLE key_cache_checks(epoch INTEGER PRIMARY KEY, check_value BLOB NOT NULL);
CREATE TABLE compose_drafts(local_order INTEGER PRIMARY KEY AUTOINCREMENT, draft_id TEXT NOT NULL UNIQUE,
  conversation_id TEXT NOT NULL UNIQUE, text TEXT NOT NULL, recipients TEXT NOT NULL,
  attachment_ids TEXT NOT NULL, route TEXT, revision INTEGER NOT NULL);
";

/// Version 5 -> 6: private attachment metadata and message/attachment links.
const MIGRATION_6: &str = "
CREATE TABLE attachments(attachment_id TEXT PRIMARY KEY,
  state TEXT NOT NULL CHECK(state IN ('pending_upload','uploaded','pending_download','available')),
  media_type TEXT NOT NULL, display_name TEXT NOT NULL, plaintext_bytes INTEGER NOT NULL,
  ciphertext_bytes INTEGER NOT NULL, ciphertext_sha256 TEXT NOT NULL, stream_version INTEGER NOT NULL,
  file_key BLOB NOT NULL, remote_object_id TEXT);
CREATE TABLE message_attachments(message_id TEXT NOT NULL, position INTEGER NOT NULL,
  attachment_id TEXT NOT NULL, PRIMARY KEY(message_id, position));
CREATE INDEX message_attachments_attachment ON message_attachments(attachment_id);
";

/// Version 6 -> 7: historical markers and staged snapshot import.
const MIGRATION_7: &str = "
ALTER TABLE journal ADD COLUMN historical INTEGER NOT NULL DEFAULT 0;
ALTER TABLE command_ledger ADD COLUMN historical INTEGER NOT NULL DEFAULT 0;
CREATE TABLE snapshot_session(id INTEGER PRIMARY KEY CHECK(id=1), generation INTEGER NOT NULL,
  high_water INTEGER NOT NULL, expected INTEGER NOT NULL, received INTEGER NOT NULL,
  last_cursor INTEGER NOT NULL, state TEXT NOT NULL CHECK(state IN ('staging','failed')));
CREATE TABLE snapshot_staging(cursor INTEGER PRIMARY KEY, wire BLOB NOT NULL);
";

/// Version 7 -> 8: generation-keyed shadow staging and outbox conflicts. Only unpublished
/// v7 staging (re-fetchable server data) is dropped; live state is untouched.
const MIGRATION_8: &str = "
DROP TABLE snapshot_staging;
DROP TABLE snapshot_session;
CREATE TABLE snapshot_generations(generation INTEGER PRIMARY KEY, high_water INTEGER NOT NULL,
  expected INTEGER NOT NULL, received INTEGER NOT NULL, received_bytes INTEGER NOT NULL,
  overlap INTEGER NOT NULL, last_cursor INTEGER NOT NULL, drained INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL CHECK(state IN ('staging','published','failed','obsolete')));
CREATE TABLE snapshot_records(generation INTEGER NOT NULL, cursor INTEGER NOT NULL, wire BLOB NOT NULL,
  PRIMARY KEY(generation, cursor));
CREATE TABLE outbox_conflicts(envelope_id TEXT PRIMARY KEY, cursor INTEGER NOT NULL);
";

fn apply_database_key(conn: &Connection, key: &DatabaseKey) -> Result<(), Error> {
    let mut hex = Zeroizing::new(String::with_capacity(64));
    for byte in key.0.iter() {
        hex.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        hex.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
    }
    let mut pragma = Zeroizing::new(String::with_capacity(80));
    pragma.push_str("PRAGMA key = \"x'");
    pragma.push_str(&hex);
    pragma.push_str("'\";");
    conn.execute_batch(&pragma)?;
    // Plain SQLite silently ignores `PRAGMA key`; refuse to continue without SQLCipher.
    let cipher: Option<String> = conn
        .query_row("PRAGMA cipher_version", [], |r| r.get(0))
        .optional()?;
    if cipher.is_none_or(|version| version.is_empty()) {
        return Err(Error::Database);
    }
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .map_err(|_| Error::WrongDatabaseKey)?;
    conn.execute_batch(
        "PRAGMA cipher_memory_security = ON; PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;",
    )?;
    Ok(())
}

fn initialize(store: &mut Store) -> Result<(), Error> {
    let conn = &mut store.conn;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_meta(version INTEGER NOT NULL)")?;
    let version: Option<i64> = conn
        .query_row("SELECT version FROM schema_meta", [], |r| r.get(0))
        .optional()?;
    match version {
        None => {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(MIGRATION_5)?;
            tx.execute_batch(MIGRATION_6)?;
            tx.execute_batch(MIGRATION_7)?;
            tx.execute_batch(MIGRATION_8)?;
            tx.execute(
                "INSERT INTO schema_meta(version) VALUES(?)",
                params![SCHEMA_VERSION],
            )?;
            tx.commit()?;
        }
        // Additive, non-destructive upgrade of the SMS checkpoint schema.
        Some(version @ (4..=7)) => {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if version <= 4 {
                tx.execute_batch(MIGRATION_5)?;
            }
            if version <= 5 {
                tx.execute_batch(MIGRATION_6)?;
            }
            if version <= 6 {
                tx.execute_batch(MIGRATION_7)?;
            }
            tx.execute_batch(MIGRATION_8)?;
            tx.execute("UPDATE schema_meta SET version=?", params![SCHEMA_VERSION])?;
            tx.commit()?;
        }
        Some(SCHEMA_VERSION) => {}
        Some(_) => return Err(Error::UnsupportedSchema),
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (key, value) in [
        ("vault_id", store.config.vault_id.to_string()),
        ("device_id", store.config.device_id.to_string()),
    ] {
        match get_meta(&tx, key)? {
            Some(existing) if existing != value => return Err(Error::IdentityMismatch),
            Some(_) => {}
            None => set_meta(&tx, key, &value)?,
        }
    }
    store.active_epoch = get_meta(&tx, "active_epoch")?
        .map(|epoch| epoch.parse().map_err(|_| Error::Database))
        .transpose()?;
    // Any attempt not finished by the previous owner may or may not have reached the carrier.
    let unfinished: Vec<String> = {
        let mut query = tx.prepare("SELECT command_id FROM attempts WHERE state IN (?,?)")?;
        query
            .query_map(
                params![
                    state_code(SendState::AttemptRecorded),
                    state_code(SendState::SubmittedToOs)
                ],
                |r| r.get(0),
            )?
            .collect::<Result<Vec<_>, _>>()?
    };
    let ctx = Ctx {
        vault_id: store.config.vault_id,
        device_id: store.config.device_id,
        keys: &store.keys,
        active_epoch: store.active_epoch,
    };
    for id in unfinished {
        tx.execute(
            "UPDATE attempts SET state=? WHERE command_id=?",
            params![state_code(SendState::OutcomeUnknown), id],
        )?;
        emit_status(&tx, &ctx, parse(&id)?, SendState::OutcomeUnknown)?;
    }
    tx.commit()?;
    Ok(())
}

/// Classifies a record at ingest time without decrypting it.
fn classify(
    conn: &Connection,
    ctx: &Ctx,
    cursor: i64,
    wire: &[u8],
    envelope: Option<&Envelope>,
) -> Result<IngestResult, Error> {
    use QuarantineReason as Q;
    let Some(envelope) = envelope.filter(|e| e.validate().is_ok()) else {
        return Ok(IngestResult::Quarantined(Q::MalformedEnvelope));
    };
    if envelope.vault_id != ctx.vault_id {
        return Ok(IngestResult::Quarantined(Q::WrongVault));
    }
    let id = envelope.envelope_id.to_string();
    let own: Option<Option<Vec<u8>>> = conn
        .query_row(
            "SELECT wire FROM outbox WHERE envelope_id=?",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(own) = own {
        if own.as_deref() != Some(wire) {
            // The server accepted different bytes under this device's envelope ID (e.g. the
            // database was rolled back before sealing/acknowledgment). Keep the local row, stop
            // uploading/sealing it, and surface it via `outbox_conflicts`.
            conn.execute(
                "INSERT OR IGNORE INTO outbox_conflicts(envelope_id,cursor) VALUES(?,?)",
                params![id, cursor],
            )?;
            return Ok(IngestResult::Quarantined(Q::EnvelopeConflict));
        }
        // The server echo proves acceptance of this device's own envelope.
        conn.execute(
            "UPDATE outbox SET state='acknowledged' WHERE envelope_id=?",
            params![id],
        )?;
        return Ok(IngestResult::Duplicate);
    }
    let mut query = conn.prepare("SELECT wire FROM journal WHERE envelope_id=?")?;
    let earlier = query
        .query_map(params![id], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if earlier.iter().any(|earlier| earlier == wire) {
        Ok(IngestResult::Duplicate)
    } else if earlier.is_empty() {
        Ok(IngestResult::Journaled)
    } else {
        Ok(IngestResult::Quarantined(Q::EnvelopeConflict))
    }
}

/// Applies one decrypted record. `Ok(Some(reason))` means quarantine; the caller rolls back.
fn apply_record(
    conn: &Connection,
    ctx: &Ctx,
    wire: &[u8],
    historical: bool,
) -> Result<Option<QuarantineReason>, Error> {
    use QuarantineReason as Q;
    let Ok(envelope) = serde_json::from_slice::<Envelope>(wire) else {
        return Ok(Some(Q::MalformedEnvelope));
    };
    let Some(keys) = ctx.keys.get(&envelope.key_epoch) else {
        return Ok(Some(Q::ProfileMismatch));
    };
    if envelope.profile_fingerprint != keys.fingerprint
        || envelope.crypto_suite != keys.profile.crypto_suite
    {
        return Ok(Some(Q::ProfileMismatch));
    }
    let (Some(sealed), Ok(aad)) = (open_frame(&envelope.ciphertext), envelope.aad_bytes()) else {
        return Ok(Some(Q::MalformedEnvelope));
    };
    let key = match envelope.purpose {
        EnvelopePurpose::Command => &keys.command,
        EnvelopePurpose::Event => &keys.event,
    };
    let Ok(plain) = decrypt(key, &aad, &sealed).map(Zeroizing::new) else {
        return Ok(Some(Q::AuthenticationFailed));
    };
    let Ok(payload) = serde_json::from_slice::<PrivatePayload>(&plain) else {
        return Ok(Some(Q::InvalidPayload));
    };
    let producer = envelope.producer_device_id;
    // MMS variants share the SMS rules plus private media metadata.
    let (payload, media) = match payload {
        PrivatePayload::MmsMessage { message, media } => (PrivatePayload::Message(message), media),
        PrivatePayload::SendMmsCommand { message, media } => {
            (PrivatePayload::SendCommand { message }, media)
        }
        other => (other, Vec::new()),
    };
    match (envelope.purpose, payload) {
        (EnvelopePurpose::Event, PrivatePayload::Message(message)) => {
            if !valid_message(&message, producer) || !media_matches(&message, &media) {
                return Ok(Some(Q::InvalidPayload));
            }
            if !store_remote_media(conn, &media)?
                || insert_message(conn, &message)? == Inserted::Conflict
            {
                return Ok(Some(Q::PayloadConflict));
            }
        }
        (EnvelopePurpose::Command, PrivatePayload::SendCommand { message }) => {
            let (Some(command_id), Some(route)) = (envelope.command_id, envelope.route.as_ref())
            else {
                return Ok(Some(Q::MalformedEnvelope));
            };
            if message.direction != Direction::Outgoing
                || !valid_message(&message, producer)
                || !media_matches(&message, &media)
            {
                return Ok(Some(Q::InvalidPayload));
            }
            if !store_remote_media(conn, &media)?
                || insert_message(conn, &message)? == Inserted::Conflict
                || !record_command(
                    conn,
                    command_id,
                    message.record.message_id,
                    route.gateway_device_id,
                )?
            {
                return Ok(Some(Q::PayloadConflict));
            }
            if route.gateway_device_id == ctx.device_id {
                let encoded = json(&message)?;
                // Retired-epoch commands are rejected immediately; they are never executable.
                let retired = ctx
                    .active_epoch
                    .is_some_and(|active| envelope.key_epoch < active);
                let inserted = conn.execute(
                    "INSERT INTO command_ledger(command_id,key_epoch,subscription_id,payload,retired,historical) VALUES(?,?,?,?,?,?) ON CONFLICT(command_id) DO NOTHING",
                    params![command_id.to_string(), envelope.key_epoch, route.subscription_id, encoded, retired, historical],
                )?;
                // Historical (snapshot) commands get no receipt: the gateway never persisted them live.
                if inserted == 1 && !historical {
                    let receipt = if retired {
                        SendState::FailedBeforeSubmission
                    } else {
                        SendState::PersistedGateway
                    };
                    emit_status(conn, ctx, command_id, receipt)?;
                }
            }
        }
        (EnvelopePurpose::Event, PrivatePayload::SendStatus { command_id, state }) => {
            // Statuses are ordered by producer sequence, stored as SQLite INTEGER.
            if !gateway_reportable(state) || i64::try_from(envelope.producer_sequence.0).is_err() {
                return Ok(Some(Q::InvalidPayload));
            }
            upsert_status(
                conn,
                command_id,
                producer,
                envelope.producer_sequence.0,
                state,
            )?;
        }
        (EnvelopePurpose::Event, PrivatePayload::ReadState { message_id }) => {
            record_seen(conn, message_id)?;
        }
        _ => return Ok(Some(Q::InvalidPayload)),
    }
    Ok(None)
}

#[derive(Debug, PartialEq, Eq)]
enum Inserted {
    New,
    Same,
    Conflict,
}
fn insert_message(conn: &Connection, message: &MessagePayload) -> Result<Inserted, Error> {
    let encoded = json(message)?;
    let id = message.record.message_id.to_string();
    let existing: Option<String> = conn
        .query_row(
            "SELECT payload FROM messages WHERE id=?",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        return Ok(if existing == encoded {
            Inserted::Same
        } else {
            Inserted::Conflict
        });
    }
    let read_elsewhere = conn
        .query_row(
            "SELECT 1 FROM seen_messages WHERE id=?",
            params![id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let seen = read_elsewhere || message.imported || message.direction == Direction::Outgoing;
    let inserted = conn.execute(
        "INSERT INTO messages(id,conversation_id,source_device_id,source_sequence,provider_message_id,payload,seen) VALUES(?,?,?,?,?,?,?)",
        params![
            id,
            message.record.conversation_id.to_string(),
            message.source_device_id.to_string(),
            to_i64(message.record.source_sequence.0)?,
            message.provider_message_id,
            encoded,
            seen
        ],
    );
    match inserted {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(failure, _))
            if failure.code == ErrorCode::ConstraintViolation =>
        {
            return Ok(Inserted::Conflict);
        }
        Err(error) => return Err(error.into()),
    }
    for (position, reference) in message.record.attachments.iter().enumerate() {
        conn.execute(
            "INSERT INTO message_attachments(message_id,position,attachment_id) VALUES(?,?,?)",
            params![
                id,
                to_i64(position as u64)?,
                reference.attachment_id.to_string()
            ],
        )?;
    }
    let address = match message.direction {
        Direction::Incoming => message.sender_address.as_ref(),
        Direction::Outgoing => message
            .recipients
            .first()
            .filter(|_| message.recipients.len() == 1),
    };
    if let Some(address) = address {
        conn.execute(
            "INSERT OR IGNORE INTO addresses(address,conversation_id) VALUES(?,?)",
            params![address, message.record.conversation_id.to_string()],
        )?;
    }
    Ok(Inserted::New)
}

/// Returns false when the command ID is already bound to a different message or gateway.
fn record_command(
    conn: &Connection,
    command_id: CommandId,
    message_id: MessageId,
    gateway: DeviceId,
) -> Result<bool, Error> {
    let (command_id, message_id, gateway) = (
        command_id.to_string(),
        message_id.to_string(),
        gateway.to_string(),
    );
    let existing: Option<(String, String)> = conn
        .query_row(
            "SELECT message_id,gateway_device_id FROM commands WHERE command_id=?",
            params![command_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match existing {
        Some(existing) => Ok(existing == (message_id, gateway)),
        None => {
            conn.execute(
                "INSERT INTO commands(command_id,message_id,gateway_device_id) VALUES(?,?,?)",
                params![command_id, message_id, gateway],
            )?;
            Ok(true)
        }
    }
}

fn record_seen(conn: &Connection, message_id: MessageId) -> Result<(), Error> {
    let id = message_id.to_string();
    conn.execute(
        "INSERT OR IGNORE INTO seen_messages(id) VALUES(?)",
        params![id],
    )?;
    conn.execute("UPDATE messages SET seen=1 WHERE id=?", params![id])?;
    Ok(())
}

/// Later statuses from the same producer win, ordered by its durable producer sequence.
fn upsert_status(
    conn: &Connection,
    command_id: CommandId,
    producer: DeviceId,
    producer_sequence: u64,
    state: SendState,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO send_status(command_id,producer_device_id,producer_sequence,state) VALUES(?,?,?,?)
         ON CONFLICT(command_id,producer_device_id) DO UPDATE SET producer_sequence=excluded.producer_sequence, state=excluded.state
         WHERE excluded.producer_sequence > send_status.producer_sequence",
        params![
            command_id.to_string(),
            producer.to_string(),
            to_i64(producer_sequence)?,
            state_code(state)
        ],
    )?;
    Ok(())
}

fn emit_status(
    conn: &Connection,
    ctx: &Ctx,
    command_id: CommandId,
    state: SendState,
) -> Result<(), Error> {
    let (_, sequence) = enqueue(
        conn,
        ctx,
        EnvelopePurpose::Event,
        None,
        None,
        &PrivatePayload::SendStatus { command_id, state },
    )?;
    upsert_status(conn, command_id, ctx.device_id, sequence, state)
}

/// Allocates the durable producer sequence and envelope ID, stores the private payload, and seals
/// immediately when the active epoch is unlocked.
fn enqueue(
    conn: &Connection,
    ctx: &Ctx,
    purpose: EnvelopePurpose,
    command_id: Option<CommandId>,
    route: Option<&GatewayRoute>,
    payload: &PrivatePayload,
) -> Result<(EnvelopeId, u64), Error> {
    let sequence = get_meta(conn, "producer_sequence")?
        .map(|value| value.parse::<u64>().map_err(|_| Error::Database))
        .transpose()?
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(Error::Database)?;
    set_meta(conn, "producer_sequence", &sequence.to_string())?;
    let envelope_id = EnvelopeId::new();
    let plain = Zeroizing::new(serde_json::to_vec(payload).map_err(|_| Error::Database)?);
    conn.execute(
        "INSERT INTO outbox(seq,envelope_id,command_id,purpose,route,plain,state) VALUES(?,?,?,?,?,?,'unsealed')",
        params![
            to_i64(sequence)?,
            envelope_id.to_string(),
            command_id.map(|id| id.to_string()),
            purpose_code(purpose),
            route.map(json).transpose()?,
            plain.as_slice()
        ],
    )?;
    seal_pending(conn, ctx, MAX_SEAL_BATCH)?;
    Ok((envelope_id, sequence))
}

fn seal_with_store(store: &mut Store) -> Result<(), Error> {
    seal_store_batch(store, MAX_SEAL_BATCH).map(|_| ())
}
fn seal_store_batch(store: &mut Store, limit: usize) -> Result<usize, Error> {
    let (conn, ctx) = store.parts();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let sealed = seal_pending(&tx, &ctx, limit)?;
    tx.commit()?;
    Ok(sealed)
}

/// Encrypts up to `limit` sealable unsealed outbox rows (oldest first) exactly once under the
/// active epoch. Rows held for media uploads are skipped. Returns the number sealed.
fn seal_pending(conn: &Connection, ctx: &Ctx, limit: usize) -> Result<usize, Error> {
    let Some(epoch) = ctx.active_epoch else {
        return Ok(0);
    };
    let Some(keys) = ctx.keys.get(&epoch) else {
        return Ok(0);
    };
    let mut count = 0;
    type Row = (i64, String, Option<String>, String, Option<String>, Vec<u8>);
    let rows: Vec<Row> = {
        let mut query = conn.prepare(
            "SELECT seq,envelope_id,command_id,purpose,route,plain FROM outbox WHERE state='unsealed' AND envelope_id NOT IN (SELECT envelope_id FROM outbox_conflicts) ORDER BY seq",
        )?;
        query
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (sequence, envelope_id, command_id, purpose, route, plain) in rows {
        if count >= limit {
            break;
        }
        // MMS rows are held until every object has a remote ID, then media metadata is resolved
        // from SQLCipher at seal time.
        let plain = match decode::<PrivatePayload>(&plain)? {
            PrivatePayload::MmsMessage { message, .. } => match resolve_media(conn, &message)? {
                Some(media) => serde_json::to_vec(&PrivatePayload::MmsMessage { message, media }),
                None => continue,
            },
            PrivatePayload::SendMmsCommand { message, .. } => {
                match resolve_media(conn, &message)? {
                    Some(media) => {
                        serde_json::to_vec(&PrivatePayload::SendMmsCommand { message, media })
                    }
                    None => continue,
                }
            }
            _ => Ok(plain),
        }
        .map_err(|_| Error::Database)?;
        let plain = Zeroizing::new(plain);
        let purpose = match purpose.as_str() {
            "command" => EnvelopePurpose::Command,
            "event" => EnvelopePurpose::Event,
            _ => return Err(Error::Database),
        };
        let mut envelope = Envelope {
            protocol_version: PROTOCOL_VERSION,
            envelope_id: parse(&envelope_id)?,
            command_id: command_id.map(|id| parse(&id)).transpose()?,
            vault_id: ctx.vault_id,
            producer_device_id: ctx.device_id,
            producer_sequence: SourceSequence(
                u64::try_from(sequence).map_err(|_| Error::Database)?,
            ),
            key_epoch: epoch,
            crypto_suite: keys.profile.crypto_suite,
            profile_fingerprint: keys.fingerprint.clone(),
            purpose,
            route: route.map(|route| decode(route.as_bytes())).transpose()?,
            ciphertext: Vec::new(),
        };
        let aad = envelope.aad_bytes().map_err(|_| Error::Database)?;
        let key = match purpose {
            EnvelopePurpose::Command => &keys.command,
            EnvelopePurpose::Event => &keys.event,
        };
        let sealed = encrypt(key, &aad, &plain).map_err(|_| Error::Crypto)?;
        envelope.ciphertext = seal_frame(&sealed);
        envelope.validate().map_err(|_| Error::Database)?;
        conn.execute(
            "UPDATE outbox SET wire=?, plain=NULL, state='queued' WHERE seq=?",
            params![
                serde_json::to_vec(&envelope).map_err(|_| Error::Database)?,
                sequence
            ],
        )?;
        count += 1;
    }
    Ok(count)
}

fn next_source_sequence(
    conn: &Connection,
    conversation_id: ConversationId,
    device_id: DeviceId,
) -> Result<SourceSequence, Error> {
    let last: i64 = conn.query_row(
        "SELECT COALESCE(MAX(source_sequence),0) FROM messages WHERE conversation_id=? AND source_device_id=?",
        params![conversation_id.to_string(), device_id.to_string()],
        |r| r.get(0),
    )?;
    let last = u64::try_from(last).map_err(|_| Error::Database)?;
    Ok(SourceSequence(last.checked_add(1).ok_or(Error::Database)?))
}

fn attempt_state(conn: &Connection, command_id: CommandId) -> Result<Option<SendState>, Error> {
    conn.query_row(
        "SELECT state FROM attempts WHERE command_id=?",
        params![command_id.to_string()],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|state| decode_state(&state))
    .transpose()
}

fn pinned_profile(conn: &Connection, epoch: u32) -> Result<Option<KeyProfile>, Error> {
    conn.query_row(
        "SELECT profile FROM key_epoch_profiles WHERE epoch=?",
        params![epoch],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|profile| decode(profile.as_bytes()))
    .transpose()
}

fn receive_cursor(conn: &Connection) -> Result<u64, Error> {
    get_meta(conn, "receive_cursor")?
        .map(|value| value.parse().map_err(|_| Error::Database))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>, Error> {
    Ok(conn
        .query_row("SELECT v FROM metadata WHERE k=?", params![key], |r| {
            r.get(0)
        })
        .optional()?)
}
fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO metadata(k,v) VALUES(?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        params![key, value],
    )?;
    Ok(())
}

fn valid_message(message: &MessagePayload, producer: DeviceId) -> bool {
    let address_ok = |address: &str| !address.is_empty() && address.len() <= MAX_ADDRESS_BYTES;
    message.source_device_id == producer
        && message.record.source_sequence.0 >= 1
        && i64::try_from(message.record.source_sequence.0).is_ok()
        && match message.transport {
            Transport::Sms => message.record.attachments.is_empty(),
            Transport::Mms => (1..=MAX_MMS_ATTACHMENTS).contains(&message.record.attachments.len()),
            Transport::Rcs => false,
        }
        && message.body.len() <= MAX_BODY_BYTES
        && message
            .provider_message_id
            .as_ref()
            .is_none_or(|id| !id.is_empty() && id.len() <= MAX_PROVIDER_ID_BYTES)
        && match message.direction {
            Direction::Incoming => message.sender_address.as_deref().is_some_and(address_ok),
            Direction::Outgoing => {
                let limit = match message.transport {
                    Transport::Mms => MAX_MMS_RECIPIENTS,
                    _ => 1,
                };
                (1..=limit).contains(&message.recipients.len())
                    && message.recipients.iter().all(|a| address_ok(a))
            }
        }
}
/// Media must describe exactly the referenced attachments, in order, and only for MMS.
fn media_matches(message: &MessagePayload, media: &[MediaDescriptor]) -> bool {
    let ids: Vec<_> = message
        .record
        .attachments
        .iter()
        .map(|a| a.attachment_id)
        .collect();
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    (message.transport == Transport::Mms) == !media.is_empty()
        && unique.len() == ids.len()
        && media.len() == ids.len()
        && media
            .iter()
            .zip(&ids)
            .all(|(descriptor, id)| descriptor.attachment_id == *id && descriptor.is_valid())
}
fn require_address(address: &str) -> Result<(), Error> {
    if address.is_empty() || address.len() > MAX_ADDRESS_BYTES {
        return Err(Error::InvalidRequest("address"));
    }
    Ok(())
}
fn require_body(body: &str, allow_empty: bool) -> Result<(), Error> {
    if (!allow_empty && body.is_empty()) || body.len() > MAX_BODY_BYTES {
        return Err(Error::InvalidRequest("body"));
    }
    Ok(())
}

fn gateway_reportable(state: SendState) -> bool {
    !matches!(
        state,
        SendState::QueuedLocal | SendState::AcceptedServer | SendState::AttemptRecorded
    )
}
fn legal_transition(from: SendState, to: SendState) -> bool {
    use SendState::*;
    matches!(
        (from, to),
        (
            AttemptRecorded,
            SubmittedToOs | Sent | Delivered | FailedBeforeSubmission | FailedConfirmed
        ) | (SubmittedToOs, Sent | Delivered | FailedConfirmed)
            | (Sent, Delivered)
            | (OutcomeUnknown, Sent | Delivered | FailedConfirmed)
    )
}
fn state_code(state: SendState) -> &'static str {
    match state {
        SendState::QueuedLocal => "queued_local",
        SendState::AcceptedServer => "accepted_server",
        SendState::PersistedGateway => "persisted_gateway",
        SendState::AttemptRecorded => "attempt_recorded",
        SendState::SubmittedToOs => "submitted_to_os",
        SendState::Sent => "sent",
        SendState::Delivered => "delivered",
        SendState::FailedBeforeSubmission => "failed_before_submission",
        SendState::FailedConfirmed => "failed_confirmed",
        SendState::OutcomeUnknown => "outcome_unknown",
    }
}
fn decode_state(code: &str) -> Result<SendState, Error> {
    Ok(match code {
        "queued_local" => SendState::QueuedLocal,
        "accepted_server" => SendState::AcceptedServer,
        "persisted_gateway" => SendState::PersistedGateway,
        "attempt_recorded" => SendState::AttemptRecorded,
        "submitted_to_os" => SendState::SubmittedToOs,
        "sent" => SendState::Sent,
        "delivered" => SendState::Delivered,
        "failed_before_submission" => SendState::FailedBeforeSubmission,
        "failed_confirmed" => SendState::FailedConfirmed,
        "outcome_unknown" => SendState::OutcomeUnknown,
        _ => return Err(Error::Database),
    })
}
fn purpose_code(purpose: EnvelopePurpose) -> &'static str {
    match purpose {
        EnvelopePurpose::Command => "command",
        EnvelopePurpose::Event => "event",
    }
}

fn seal_frame(value: &EncryptedEnvelope) -> Vec<u8> {
    let mut out = Vec::with_capacity(NONCE_BYTES + value.ciphertext.len());
    out.extend_from_slice(&value.nonce);
    out.extend_from_slice(&value.ciphertext);
    out
}
fn open_frame(value: &[u8]) -> Option<EncryptedEnvelope> {
    if !(AEAD_FRAME_MIN_BYTES..=MAX_CIPHERTEXT_BYTES).contains(&value.len()) {
        return None;
    }
    let (nonce, ciphertext) = value.split_at(NONCE_BYTES);
    Some(EncryptedEnvelope {
        nonce: nonce.try_into().ok()?,
        ciphertext: ciphertext.to_vec(),
    })
}

struct AttachmentRow {
    state: AttachmentState,
    media_type: String,
    display_name: String,
    plaintext_bytes: u64,
    ciphertext_bytes: u64,
    ciphertext_sha256: String,
    file_key: Zeroizing<[u8; 32]>,
    remote_object_id: Option<String>,
}
impl AttachmentRow {
    fn info(&self, attachment_id: AttachmentId) -> AttachmentInfo {
        AttachmentInfo {
            attachment_id,
            media_type: self.media_type.clone(),
            display_name: self.display_name.clone(),
            plaintext_bytes: self.plaintext_bytes,
            ciphertext_bytes: self.ciphertext_bytes,
            ciphertext_sha256: self.ciphertext_sha256.clone(),
            state: self.state,
        }
    }
    fn file_key(&self) -> Result<FileKey, Error> {
        FileKey::import_encrypted_reference(*self.file_key).map_err(|_| Error::Crypto)
    }
}
fn attachment_row(conn: &Connection, id: AttachmentId) -> Result<Option<AttachmentRow>, Error> {
    type Raw = (
        String,
        String,
        String,
        i64,
        i64,
        String,
        Vec<u8>,
        Option<String>,
    );
    let row: Option<Raw> = conn
        .query_row(
            "SELECT state,media_type,display_name,plaintext_bytes,ciphertext_bytes,ciphertext_sha256,file_key,remote_object_id FROM attachments WHERE attachment_id=?",
            params![id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
        )
        .optional()?;
    let Some((state, media_type, display_name, plain, cipher, sha, key, remote)) = row else {
        return Ok(None);
    };
    let key = Zeroizing::new(key);
    Ok(Some(AttachmentRow {
        state: AttachmentState::from_code(&state)?,
        media_type,
        display_name,
        plaintext_bytes: u64::try_from(plain).map_err(|_| Error::Database)?,
        ciphertext_bytes: u64::try_from(cipher).map_err(|_| Error::Database)?,
        ciphertext_sha256: sha,
        file_key: Zeroizing::new(key.as_slice().try_into().map_err(|_| Error::Database)?),
        remote_object_id: remote,
    }))
}
/// References for attachments prepared (encrypted) on this device; anything else is refused.
fn local_references(
    conn: &Connection,
    ids: &[AttachmentId],
) -> Result<Vec<AttachmentReference>, Error> {
    let mut seen = Vec::with_capacity(ids.len());
    for id in ids {
        let prepared_here = attachment_row(conn, *id)?.is_some_and(|row| {
            matches!(
                row.state,
                AttachmentState::PendingUpload | AttachmentState::Uploaded
            )
        });
        if !prepared_here || seen.contains(id) {
            return Err(Error::InvalidRequest(
                "attachment not prepared on this device",
            ));
        }
        seen.push(*id);
    }
    Ok(seen
        .into_iter()
        .map(|attachment_id| AttachmentReference {
            attachment_id,
            pending: false,
        })
        .collect())
}
/// Private descriptors for a message, or `None` while any object still lacks a remote ID.
fn resolve_media(
    conn: &Connection,
    message: &MessagePayload,
) -> Result<Option<Vec<MediaDescriptor>>, Error> {
    let mut media = Vec::with_capacity(message.record.attachments.len());
    for reference in &message.record.attachments {
        let row = attachment_row(conn, reference.attachment_id)?.ok_or(Error::Database)?;
        let Some(remote_object_id) = row.remote_object_id.clone() else {
            return Ok(None);
        };
        media.push(MediaDescriptor {
            attachment_id: reference.attachment_id,
            remote_object_id,
            media_type: row.media_type.clone(),
            display_name: row.display_name.clone(),
            plaintext_bytes: row.plaintext_bytes,
            ciphertext_bytes: row.ciphertext_bytes,
            ciphertext_sha256: row.ciphertext_sha256.clone(),
            stream_version: STREAM_VERSION,
            file_key: *row.file_key,
        });
    }
    Ok(Some(media))
}
/// Records received metadata as `pending_download`; false if an ID is bound to other metadata.
fn store_remote_media(conn: &Connection, media: &[MediaDescriptor]) -> Result<bool, Error> {
    for descriptor in media {
        if let Some(row) = attachment_row(conn, descriptor.attachment_id)? {
            let same = row.remote_object_id.as_deref()
                == Some(descriptor.remote_object_id.as_str())
                && row.ciphertext_sha256 == descriptor.ciphertext_sha256
                && row.ciphertext_bytes == descriptor.ciphertext_bytes
                && row.plaintext_bytes == descriptor.plaintext_bytes
                && row.media_type == descriptor.media_type
                && row.display_name == descriptor.display_name
                && same_bytes(row.file_key.as_slice(), &descriptor.file_key);
            if !same {
                return Ok(false);
            }
            continue;
        }
        conn.execute(
            "INSERT INTO attachments(attachment_id,state,media_type,display_name,plaintext_bytes,ciphertext_bytes,ciphertext_sha256,stream_version,file_key,remote_object_id) VALUES(?,?,?,?,?,?,?,?,?,?)",
            params![
                descriptor.attachment_id.to_string(),
                AttachmentState::PendingDownload.code(),
                descriptor.media_type,
                descriptor.display_name,
                to_i64(descriptor.plaintext_bytes)?,
                to_i64(descriptor.ciphertext_bytes)?,
                descriptor.ciphertext_sha256,
                descriptor.stream_version,
                descriptor.file_key.as_slice(),
                descriptor.remote_object_id
            ],
        )?;
    }
    Ok(true)
}
/// Sets each reference's `pending` from local verification state; returns whether all are local.
fn localize(
    conn: &Connection,
    mut message: MessagePayload,
) -> Result<(MessagePayload, bool), Error> {
    let mut all_local = true;
    for reference in &mut message.record.attachments {
        let local =
            attachment_row(conn, reference.attachment_id)?.is_some_and(|row| row.state.is_local());
        reference.pending = !local;
        all_local &= local;
    }
    Ok((message, all_local))
}

fn same_key(left: &[u8; 32], right: &[u8; 32]) -> bool {
    same_bytes(left, right)
}
/// Constant-time for equal lengths.
fn same_bytes(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (l, r)| acc | (l ^ r))
            == 0
}

fn key_cache_bytes(
    config: &ClientConfig,
    epoch: u32,
    keys: &EpochKeys,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    if keys.fingerprint.len() != FINGERPRINT_HEX_BYTES {
        return Err(Error::Crypto);
    }
    let mut out = Zeroizing::new(Vec::with_capacity(KEY_CACHE_BYTES));
    out.extend_from_slice(KEY_CACHE_MAGIC);
    out.push(KEY_CACHE_VERSION);
    out.extend_from_slice(config.vault_id.0.as_bytes());
    out.extend_from_slice(config.device_id.0.as_bytes());
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(keys.fingerprint.as_bytes());
    keys.command
        .with_native_cache_bytes(|bytes| out.extend_from_slice(bytes));
    keys.event
        .with_native_cache_bytes(|bytes| out.extend_from_slice(bytes));
    Ok(out)
}
/// SHA-256 over the complete bound cache; recorded locally at passphrase unlock.
fn key_cache_check(cache: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(KEY_CACHE_CHECK_DOMAIN);
    digest.update(cache);
    digest.finalize().into()
}

const COMPOSE_COLUMNS: &str =
    "draft_id,conversation_id,text,recipients,attachment_ids,route,revision";
type ComposeRow = (String, String, String, String, String, Option<String>, i64);
fn compose_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ComposeRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}
fn decode_compose_row(row: ComposeRow) -> Result<ComposeDraft, Error> {
    let (draft_id, conversation_id, text, recipients, attachment_ids, route, revision) = row;
    Ok(ComposeDraft {
        draft_id: parse(&draft_id)?,
        conversation_id: parse(&conversation_id)?,
        text,
        recipients: decode(recipients.as_bytes())?,
        attachment_ids: decode(attachment_ids.as_bytes())?,
        route: route.map(|route| decode(route.as_bytes())).transpose()?,
        revision: u64::try_from(revision).map_err(|_| Error::Database)?,
    })
}
/// `column` is one of the two unique keys, never caller input.
fn load_compose_draft(
    conn: &Connection,
    column: &'static str,
    value: &str,
) -> Result<Option<ComposeDraft>, Error> {
    conn.query_row(
        &format!("SELECT {COMPOSE_COLUMNS} FROM compose_drafts WHERE {column}=?"),
        params![value],
        compose_row,
    )
    .optional()?
    .map(decode_compose_row)
    .transpose()
}
fn canonical(path: &Path) -> Result<PathBuf, Error> {
    let parent = path.parent().ok_or(Error::Database)?;
    std::fs::create_dir_all(parent).map_err(|_| Error::Database)?;
    let parent = parent.canonicalize().map_err(|_| Error::Database)?;
    Ok(parent.join(path.file_name().ok_or(Error::Database)?))
}
fn json<T: Serialize>(value: &T) -> Result<String, Error> {
    serde_json::to_string(value).map_err(|_| Error::Database)
}
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|_| Error::Database)
}
fn parse<T: FromStr>(value: &str) -> Result<T, Error> {
    value.parse().map_err(|_| Error::Database)
}
fn to_i64(value: u64) -> Result<i64, Error> {
    i64::try_from(value).map_err(|_| Error::Database)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_key_must_be_exactly_32_bytes() {
        assert!(matches!(
            DatabaseKey::new(&[7; 31]),
            Err(Error::InvalidDatabaseKey)
        ));
        assert!(DatabaseKey::new(&[7; 32]).is_ok());
    }

    #[test]
    fn send_state_codes_round_trip_and_reject_unknown() {
        use SendState::*;
        for state in [
            QueuedLocal,
            AcceptedServer,
            PersistedGateway,
            AttemptRecorded,
            SubmittedToOs,
            Sent,
            Delivered,
            FailedBeforeSubmission,
            FailedConfirmed,
            OutcomeUnknown,
        ] {
            assert_eq!(decode_state(state_code(state)), Ok(state));
        }
        assert_eq!(decode_state("attempted"), Err(Error::Database));
    }

    #[test]
    fn version_4_database_is_upgraded_without_losing_data() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = ClientConfig {
            database_path: dir.path().join("v4.db"),
            vault_id: VaultId::new(),
            device_id: DeviceId::new(),
        };
        let conversation = ConversationId::new();
        {
            let conn = Connection::open(&config.database_path).unwrap();
            apply_database_key(&conn, &DatabaseKey::new(&[9; 32]).unwrap()).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_meta(version INTEGER NOT NULL); INSERT INTO schema_meta VALUES(4);",
            )
            .unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO drafts(conversation_id,content,revision) VALUES(?,'kept',3)",
                params![conversation.to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO command_ledger(command_id,key_epoch,subscription_id,payload) VALUES('c',1,'s','{}')",
                [],
            )
            .unwrap();
        }
        let client = Client::open(config, DatabaseKey::new(&[9; 32]).unwrap()).unwrap();
        assert_eq!(client.draft(conversation).unwrap().unwrap().revision, 3);
        assert!(client.compose_drafts().unwrap().is_empty());
        let s = client.lock().unwrap();
        let version: i64 = s
            .conn
            .query_row("SELECT version FROM schema_meta", [], |r| r.get(0))
            .unwrap();
        let retired: i64 = s
            .conn
            .query_row(
                "SELECT retired FROM command_ledger WHERE command_id='c'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let attachments: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM attachments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(attachments, 0, "migration 6 table exists");
        let historical: i64 = s
            .conn
            .query_row(
                "SELECT historical FROM command_ledger WHERE command_id='c'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            historical, 0,
            "migration 7 keeps live ledger rows executable"
        );
        assert_eq!((version, retired), (SCHEMA_VERSION, 0));
    }

    #[test]
    fn install_rejects_missing_final_tag_or_trailing_data_even_with_matching_digest() {
        use sha2::{Digest, Sha256};
        let dir = tempfile::TempDir::new().unwrap();
        let root = media::prepare_media_root(&dir.path().join("x.db")).unwrap();
        let key = FileKey::generate().unwrap();
        let id = AttachmentId::new();
        let aad = media::media_aad(VaultId::new(), id);
        let mut cipher = Vec::new();
        openpush_crypto::encrypt_stream(&[42u8; 100_000][..], &mut cipher, &key, &aad).unwrap();
        // The final frame is a 4-byte length plus a 17-byte empty TAG_FINAL frame.
        let without_final = cipher[..cipher.len() - 21].to_vec();
        let mut trailing = cipher.clone();
        trailing.push(0);
        for bad in [without_final, trailing] {
            let path = dir.path().join("download.bin");
            std::fs::write(&path, &bad).unwrap();
            let digest = media::hex(&Sha256::digest(&bad));
            let expected = media::Expected {
                ciphertext_bytes: bad.len() as u64,
                ciphertext_sha256: &digest,
                plaintext_bytes: 100_000,
            };
            let result = media::install_into_store(&root, id, &path, &expected, &key, &aad);
            assert_eq!(result, Err(Error::InvalidMedia));
            assert!(!media::cipher_path(&root, id).exists());
            for scratch in ["tmp", "plain"] {
                assert_eq!(std::fs::read_dir(root.join(scratch)).unwrap().count(), 0);
            }
        }
        let path = dir.path().join("genuine.bin");
        std::fs::write(&path, &cipher).unwrap();
        let digest = media::hex(&Sha256::digest(&cipher));
        let expected = |plaintext_bytes| media::Expected {
            ciphertext_bytes: cipher.len() as u64,
            ciphertext_sha256: &digest,
            plaintext_bytes,
        };
        // Authenticated plaintext length must match the decrypted byte count.
        assert_eq!(
            media::install_into_store(&root, id, &path, &expected(99_999), &key, &aad),
            Err(Error::InvalidMedia)
        );
        assert!(!media::cipher_path(&root, id).exists());
        media::install_into_store(&root, id, &path, &expected(100_000), &key, &aad).unwrap();
        let destination = media::cipher_path(&root, id);
        assert_eq!(std::fs::read(&destination).unwrap(), cipher);

        // A second (concurrent) installer never unlinks the verified object.
        #[cfg(unix)]
        let inode = |p: &std::path::Path| {
            std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(p).unwrap())
        };
        #[cfg(unix)]
        let first = inode(&destination);
        media::install_into_store(&root, id, &path, &expected(100_000), &key, &aad).unwrap();
        #[cfg(unix)]
        assert_eq!(inode(&destination), first);
        // A damaged leftover (not a verified object) is atomically replaced.
        std::fs::remove_file(&destination).unwrap();
        std::fs::write(&destination, b"damaged").unwrap();
        media::install_into_store(&root, id, &path, &expected(100_000), &key, &aad).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), cipher);
        assert_eq!(std::fs::read_dir(root.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn plaintext_handle_survives_reopen_and_scratch_purge_spares_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = ClientConfig {
            database_path: dir.path().join("device.db"),
            vault_id: VaultId::new(),
            device_id: DeviceId::new(),
        };
        let open = || Client::open(config.clone(), DatabaseKey::new(&[3; 32]).unwrap()).unwrap();
        let client = open();
        let source = dir.path().join("photo.bin");
        std::fs::write(&source, b"pixels").unwrap();
        let info = client
            .prepare_attachment(&source, "image/png", "photo.png")
            .unwrap();
        let plain = client.open_native_plaintext(info.attachment_id).unwrap();
        let duplicate = open();
        drop((client, duplicate));
        let reopened = open(); // the live handle keeps the owner, so nothing is purged
        assert_eq!(std::fs::read(plain.path()).unwrap(), b"pixels");
        let scratch = plain.path().parent().unwrap().to_path_buf();
        drop(plain);
        drop(reopened);

        // Fresh owner: files and symlinks go (never link targets); directories stay; open works.
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"keep").unwrap();
        std::fs::create_dir(scratch.join("unexpected")).unwrap();
        std::fs::write(scratch.join("stale.bin"), b"old plaintext").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, scratch.join("link")).unwrap();
        let fresh = open();
        assert!(scratch.join("unexpected").is_dir());
        assert!(!scratch.join("stale.bin").exists());
        assert!(std::fs::symlink_metadata(scratch.join("link")).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        assert_eq!(
            fresh.attachment_info(info.attachment_id).unwrap().state,
            AttachmentState::PendingUpload
        );
    }

    /// Apply-level MMS validation: authenticated payloads with inconsistent media are
    /// quarantined and roll back, and an attachment ID cannot be rebound to new metadata.
    #[test]
    fn received_mms_media_validation_quarantines_and_rolls_back() {
        use openpush_crypto::{create_vault_check_header, derive_root_key};
        let dir = tempfile::TempDir::new().unwrap();
        let vault_id = VaultId::new();
        let profile = KeyProfile::new(vault_id.0, 1).unwrap();
        let root = derive_root_key("correct horse battery staple", &profile).unwrap();
        let header = create_vault_check_header(&root, profile.clone()).unwrap();
        let event_key = derive_purpose_key(&root, &profile, KeyPurpose::Event).unwrap();
        let client = Client::open(
            ClientConfig {
                database_path: dir.path().join("d.db"),
                vault_id,
                device_id: DeviceId::new(),
            },
            DatabaseKey::new(&[5; 32]).unwrap(),
        )
        .unwrap();
        client
            .unlock(&profile, &header, "correct horse battery staple")
            .unwrap();
        let producer = DeviceId::new();
        let conversation = ConversationId::new();
        let mut sequence = 0;
        let mut message = |ids: &[AttachmentId]| {
            sequence += 1;
            MessagePayload {
                record: MessageRecord {
                    message_id: MessageId::new(),
                    conversation_id: conversation,
                    source_sequence: SourceSequence(sequence),
                    attachments: ids
                        .iter()
                        .map(|&attachment_id| AttachmentReference {
                            attachment_id,
                            pending: false,
                        })
                        .collect(),
                },
                source_device_id: producer,
                provider_message_id: None,
                sender_address: Some("+15555550100".into()),
                recipients: vec![],
                body: String::new(),
                transport: Transport::Mms,
                direction: Direction::Incoming,
                imported: false,
            }
        };
        let descriptor = |attachment_id, remote: &str, sha: &str| MediaDescriptor {
            attachment_id,
            remote_object_id: remote.into(),
            media_type: "image/png".into(),
            display_name: "a.png".into(),
            plaintext_bytes: 10,
            ciphertext_bytes: 100,
            ciphertext_sha256: sha.into(),
            stream_version: media::STREAM_VERSION,
            file_key: [1; 32],
        };
        let mut producer_sequence = 0;
        let mut seal = |payload: PrivatePayload| {
            producer_sequence += 1;
            let mut envelope = Envelope {
                protocol_version: PROTOCOL_VERSION,
                envelope_id: EnvelopeId::new(),
                command_id: None,
                vault_id,
                producer_device_id: producer,
                producer_sequence: SourceSequence(producer_sequence),
                key_epoch: 1,
                crypto_suite: profile.crypto_suite,
                profile_fingerprint: profile.fingerprint().unwrap(),
                purpose: EnvelopePurpose::Event,
                route: None,
                ciphertext: Vec::new(),
            };
            let plain = serde_json::to_vec(&payload).unwrap();
            let sealed = encrypt(&event_key, &envelope.aad_bytes().unwrap(), &plain).unwrap();
            envelope.ciphertext = seal_frame(&sealed);
            envelope
        };
        let (sha, other_sha) = ("a".repeat(64), "b".repeat(64));
        let (r1, r2) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let [a, b, c, d, e] = [(); 5].map(|_| AttachmentId::new());
        let records = [
            seal(PrivatePayload::MmsMessage {
                message: message(&[a]),
                media: vec![descriptor(a, &r1, &sha)],
            }),
            // Rebinding an existing attachment ID to different metadata.
            seal(PrivatePayload::MmsMessage {
                message: message(&[a]),
                media: vec![descriptor(a, &r2, &other_sha)],
            }),
            // Descriptor count mismatch.
            seal(PrivatePayload::MmsMessage {
                message: message(&[b, c]),
                media: vec![descriptor(b, &r1, &sha)],
            }),
            // Descriptor order mismatch.
            seal(PrivatePayload::MmsMessage {
                message: message(&[b, c]),
                media: vec![descriptor(c, &r1, &sha), descriptor(b, &r2, &sha)],
            }),
            // Invalid descriptor (digest not 64 lowercase hex).
            seal(PrivatePayload::MmsMessage {
                message: message(&[d]),
                media: vec![descriptor(d, &r1, "zz")],
            }),
            // MMS payload missing media.
            seal(PrivatePayload::MmsMessage {
                message: message(&[e]),
                media: vec![],
            }),
            // MMS transport smuggled through the SMS variant.
            seal(PrivatePayload::Message(message(&[e]))),
        ];
        for (index, envelope) in records.iter().enumerate() {
            assert_eq!(
                client.ingest(envelope, Cursor(index as u64 + 1)).unwrap(),
                IngestResult::Journaled
            );
        }
        let report = client.apply_pending(100).unwrap();
        assert_eq!((report.applied, report.quarantined), (1, 6));
        let reasons: Vec<_> = client
            .quarantined()
            .unwrap()
            .into_iter()
            .map(|q| q.reason)
            .collect();
        use QuarantineReason::{InvalidPayload as Invalid, PayloadConflict as Conflict};
        assert_eq!(
            reasons,
            vec![Conflict, Invalid, Invalid, Invalid, Invalid, Invalid]
        );
        assert_eq!(client.messages(conversation).unwrap().len(), 1);
        assert_eq!(
            client.attachment_info(a).unwrap().state,
            AttachmentState::PendingDownload
        );
        assert_eq!(
            client.pending_downloads().unwrap()[0]
                .remote_object_id
                .as_deref(),
            Some(r1.as_str()),
            "first binding kept"
        );
        for id in [b, c, d, e] {
            assert_eq!(
                client.attachment_info(id),
                Err(Error::NotFound),
                "rolled back"
            );
        }
    }

    #[test]
    fn batch_reads_stop_at_the_byte_budget_before_allocating() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(cursor INTEGER PRIMARY KEY, wire BLOB NOT NULL)")
            .unwrap();
        let near_limit = MAX_RAW_ENVELOPE_BYTES; // largest stored wire (oversize input -> marker)
        for cursor in 1..=6 {
            conn.execute(
                "INSERT INTO t VALUES(?, zeroblob(?))",
                params![cursor, near_limit as i64],
            )
            .unwrap();
        }
        for cursor in 7..=1_500 {
            conn.execute("INSERT INTO t VALUES(?, x'00')", params![cursor])
                .unwrap();
        }
        let batch = |after: i64, limit: i64| -> Vec<i64> {
            let mut query = conn
                .prepare(
                    "SELECT LENGTH(wire),cursor,wire FROM t WHERE cursor>? ORDER BY cursor LIMIT ?",
                )
                .unwrap();
            let rows: Vec<(i64, Vec<u8>)> = take_within_budget(
                query.query(params![after, limit]).unwrap(),
                MAX_SNAPSHOT_PAGE_BYTES,
                |r| Ok((r.get(1)?, r.get(2)?)),
            )
            .unwrap();
            let bytes: usize = rows.iter().map(|(_, w)| w.len()).sum();
            assert!(rows.len() == 1 || bytes <= MAX_SNAPSHOT_PAGE_BYTES);
            rows.into_iter().map(|(cursor, _)| cursor).collect()
        };
        // limit 1000 with near-limit records: the 8 MiB budget, not the count, ends the batch,
        // and the result is the ordered prefix.
        assert_eq!(batch(0, 1_000), vec![1, 2, 3, 4]);
        assert_eq!(
            batch(4, 1_000),
            vec![5, 6].into_iter().chain(7..=1_004).collect::<Vec<_>>()
        );
        // Small records: the count limit still applies.
        assert_eq!(batch(6, 1_000).len(), 1_000);
        // A single record larger than the budget is still taken alone (always progress).
        conn.execute(
            "INSERT INTO t VALUES(2000, zeroblob(?))",
            params![MAX_SNAPSHOT_PAGE_BYTES as i64 + 1],
        )
        .unwrap();
        conn.execute("INSERT INTO t VALUES(2001, x'00')", [])
            .unwrap();
        assert_eq!(batch(1_999, 1_000), vec![2000]);
    }

    #[test]
    fn frame_rejects_short_ciphertext() {
        assert!(open_frame(&[0; AEAD_FRAME_MIN_BYTES - 1]).is_none());
        assert!(open_frame(&[0; AEAD_FRAME_MIN_BYTES]).is_some());
    }
}
