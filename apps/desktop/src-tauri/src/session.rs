//! One native session = one origin+vault+device binding, one client-core owner, one network
//! supervisor. All functions here are synchronous (they call into client-core / the filesystem)
//! and are invoked from `spawn_blocking`, never directly on an async worker or the UI thread.
use crate::{
    credentials::{ensure_database_key, prepare_data_dir, stored_credential, Binding},
    dto::{
        self, AttachmentView, ConversationView, DraftView, GatewayView, Head, SendResultView,
        Snapshot,
    },
    error::{core_error, BridgeError, BridgeResult},
    gateways::find_route,
    media,
    net::Api,
    notifications::NotificationPreferences,
    secure_store::SecretStore,
};
use openpush_client_core::{
    AttachmentId, Client, ClientConfig, ComposeDraft, ComposeDraftUpdate, ConversationId,
    DatabaseKey, DeviceId, Direction, DraftId, GatewayRoute, Message, MessageId, NativeKeyCache,
    VaultId,
};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub type Notifier = Arc<dyn Fn() + Send + Sync>;

const MAX_ACTIVE_MESSAGES: usize = 500;
const MAX_PREVIEWS_PER_SNAPSHOT: usize = 12;
const MAX_PREVIEW_CACHE: usize = 256;
const MAX_SEEN_IDS: usize = 1000;
const MAX_ATTACHMENTS: usize = 10;
const PENDING_COUNT_CAP: usize = 200;
/// Mirrors client-core's message body and recipient limits.
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_RECIPIENTS: usize = 20;
pub const GATEWAYS_FILE: &str = "gateways.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultSummary {
    pub epoch: u32,
    pub fingerprint: String,
}

#[derive(Default)]
pub struct NetStatus {
    /// A live WebSocket is negotiated (`ready` received).
    pub live: bool,
    pub error_code: Option<&'static str>,
    /// Last non-transport outbox/media problem, shown while otherwise connected.
    pub work_error: Option<&'static str>,
    pub revoked: bool,
    pub gateways: Vec<GatewayView>,
    pub gateways_known: bool,
    pub vault: Option<VaultSummary>,
}

/// Local attachment ID -> server object ID, learned when this host uploads or downloads an
/// object. Core keeps the authoritative binding in SQLCipher but exposes it only for pending
/// work; this owner-only sidecar lets the owning device request a public copy later. It holds
/// no key material.
pub struct RemoteIds {
    path: PathBuf,
    map: HashMap<String, String>,
}
impl RemoteIds {
    fn load(path: PathBuf) -> Self {
        let map = fs::read(&path)
            .ok()
            .filter(|bytes| bytes.len() <= 4 * 1024 * 1024)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { path, map }
    }
    pub fn get(&self, local: AttachmentId) -> Option<String> {
        self.map.get(&local.to_string()).cloned()
    }
    pub fn insert(&mut self, local: AttachmentId, remote: &str) {
        if self.map.get(&local.to_string()).map(String::as_str) == Some(remote)
            || uuid::Uuid::parse_str(remote).is_err()
        {
            return;
        }
        self.map.insert(local.to_string(), remote.to_owned());
        if let Ok(bytes) = serde_json::to_vec(&self.map) {
            let _ = crate::fsutil::write_private_atomic(&self.path, &bytes);
        }
    }
}

pub struct Session {
    pub binding: Binding,
    pub client: Client,
    pub api: Api,
    pub data_dir: PathBuf,
    pub status: Mutex<NetStatus>,
    pub previews: Mutex<HashMap<AttachmentId, Option<String>>>,
    pub transfer_errors: Mutex<HashMap<AttachmentId, String>>,
    pub remote_ids: Mutex<RemoteIds>,
    /// Wakes the outbound/media worker only (send, read state, unlock).
    pub work_wake: Notify,
    /// Wakes a pending reconnect wait (for example after unlock).
    pub reconnect_wake: Notify,
    pub cancel: CancellationToken,
    pub notifier: Notifier,
    pub mismatch: AtomicBool,
    /// A staged snapshot generation known to be impossible to complete; never resumed.
    pub abandoned_snapshot: Mutex<Option<u64>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn host() -> BridgeError {
    BridgeError::host_state()
}

/// Opens (or re-attaches to) the binding's database with its existing protected key, and
/// restores cached purpose keys without a passphrase when their integrity check passes.
pub fn open_session(
    root: &Path,
    store: &dyn SecretStore,
    binding: &Binding,
    cached_epochs: &[u32],
    notifier: Notifier,
) -> BridgeResult<Session> {
    let credential = stored_credential(store, binding)?.ok_or_else(BridgeError::no_session)?;
    let data_dir = prepare_data_dir(root, binding)?;
    let database = binding.database_path(root);
    let key = ensure_database_key(store, binding, &database)?;
    let config = ClientConfig {
        database_path: database,
        vault_id: VaultId::from_str(&binding.vault_id).map_err(|_| host())?,
        device_id: DeviceId::from_str(&binding.device_id).map_err(|_| host())?,
    };
    let client =
        Client::open(config, DatabaseKey::new(&key).map_err(core_error)?).map_err(core_error)?;
    for epoch in cached_epochs {
        if let Some(bytes) = store.get(&binding.key_cache_account(*epoch))? {
            match client
                .import_native_key_cache(&NativeKeyCache::from_native_storage(bytes.to_vec()))
            {
                // A stale or unverifiable cache simply leaves this epoch locked.
                Ok(())
                | Err(
                    openpush_client_core::Error::InvalidKeyCache
                    | openpush_client_core::Error::InvalidProfile,
                ) => {}
                Err(error) => return Err(core_error(error)),
            }
        }
    }
    let api = Api::new(&credential.origin, &credential.device_token)?;
    // Last server-reported routes, so an offline send can still be checked against them.
    let gateways: Option<Vec<GatewayView>> = fs::read(data_dir.join(GATEWAYS_FILE))
        .ok()
        .filter(|b| b.len() <= 256 * 1024)
        .and_then(|b| serde_json::from_slice(&b).ok());
    let status = NetStatus {
        gateways_known: gateways.is_some(),
        gateways: gateways.unwrap_or_default(),
        ..NetStatus::default()
    };
    Ok(Session {
        binding: binding.clone(),
        client,
        api,
        remote_ids: Mutex::new(RemoteIds::load(data_dir.join("remote-objects.json"))),
        data_dir,
        status: Mutex::new(status),
        previews: Mutex::new(HashMap::new()),
        transfer_errors: Mutex::new(HashMap::new()),
        work_wake: Notify::new(),
        reconnect_wake: Notify::new(),
        cancel: CancellationToken::new(),
        notifier,
        mismatch: AtomicBool::new(false),
        abandoned_snapshot: Mutex::new(None),
    })
}

/// Draft input from the UI. Unknown fields (for example the draft's own `revision`) are ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftInput {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub recipient_ids: Vec<String>,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    #[serde(default)]
    pub gateway_id: Option<String>,
    #[serde(default)]
    pub sim_id: Option<String>,
    pub expected_revision: String,
}

fn invalid_draft(message: &'static str) -> BridgeError {
    BridgeError::new("invalid-draft", message)
}

fn parse_revision(value: &str) -> BridgeResult<u64> {
    value
        .parse()
        .map_err(|_| invalid_draft("The draft revision is invalid."))
}

/// Accepts international (`+CC…`) and national numbers and short codes; separators are removed.
pub fn normalize_recipient(value: &str) -> BridgeResult<String> {
    let compact: String = value
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .collect();
    let (plus, digits) = match compact.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, compact.as_str()),
    };
    if (3..=15).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()) {
        Ok(if plus {
            format!("+{digits}")
        } else {
            digits.to_owned()
        })
    } else {
        Err(BridgeError::new(
            "invalid-recipient",
            "Recipients must be phone numbers (international +CC format, national digits, or a short code). Other address types are not supported.",
        ))
    }
}

fn parse_route(gateway: &str, sim: &str) -> BridgeResult<GatewayRoute> {
    Ok(GatewayRoute {
        gateway_device_id: DeviceId::from_str(gateway)
            .map_err(|_| BridgeError::new("invalid-route", "The gateway ID is invalid."))?,
        subscription_id: sim.to_owned(),
    })
}

fn other_party(message: &Message) -> Vec<String> {
    match message.payload.direction {
        Direction::Incoming => message.payload.sender_address.clone().into_iter().collect(),
        Direction::Outgoing => message.payload.recipients.clone(),
    }
}

impl Session {
    pub fn request_work(&self) {
        self.work_wake.notify_one();
    }

    pub fn notify(&self) {
        (self.notifier)();
    }

    pub fn gateways(&self) -> (Vec<GatewayView>, bool) {
        let status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        (status.gateways.clone(), status.gateways_known)
    }

    /// Recipients of an existing conversation, derived from its latest stored message.
    fn conversation_recipients(&self, conversation: ConversationId) -> BridgeResult<Vec<String>> {
        Ok(self
            .client
            .messages(conversation)
            .map_err(core_error)?
            .last()
            .map(other_party)
            .unwrap_or_default())
    }

    fn check_attachments(&self, ids: &[String]) -> BridgeResult<Vec<AttachmentId>> {
        if ids.len() > MAX_ATTACHMENTS {
            return Err(BridgeError::new(
                "invalid-attachment",
                "A message can carry at most 10 attachments.",
            ));
        }
        ids.iter()
            .map(|id| {
                let id = AttachmentId::from_str(id).map_err(|_| {
                    BridgeError::new("invalid-attachment", "The attachment ID is invalid.")
                })?;
                let info = self.client.attachment_info(id).map_err(core_error)?;
                if !info.state.is_local() {
                    return Err(BridgeError::new(
                        "invalid-attachment",
                        "Only attachments added on this device can be sent.",
                    ));
                }
                Ok(id)
            })
            .collect()
    }

    /// CAS draft save. Text, recipients, attachments and route are persisted together.
    pub fn save_draft(&self, input: &DraftInput) -> BridgeResult<DraftView> {
        let expected = parse_revision(&input.expected_revision)?;
        let draft_id = match DraftId::from_str(&input.id) {
            Ok(id) => id,
            // A UI placeholder for a draft that was never stored yet.
            Err(_) => {
                let conversation = match input.conversation_id.as_str() {
                    "" => None,
                    id => Some(
                        ConversationId::from_str(id)
                            .map_err(|_| invalid_draft("The conversation ID is invalid."))?,
                    ),
                };
                self.client
                    .create_compose_draft(conversation)
                    .map_err(core_error)?
                    .draft_id
            }
        };
        let current = self
            .client
            .compose_draft(draft_id)
            .map_err(core_error)?
            .ok_or_else(|| core_error(openpush_client_core::Error::NotFound))?;
        if !input.conversation_id.is_empty()
            && input.conversation_id != current.conversation_id.to_string()
        {
            return Err(invalid_draft(
                "The draft does not belong to this conversation.",
            ));
        }
        let route = match (
            input.gateway_id.as_deref().unwrap_or(""),
            input.sim_id.as_deref().unwrap_or(""),
        ) {
            ("", "") => current.route.clone(),
            (gateway, sim) if !gateway.is_empty() && !sim.is_empty() => {
                Some(parse_route(gateway, sim)?)
            }
            _ => {
                return Err(BridgeError::new(
                    "invalid-route",
                    "Select a gateway and SIM together.",
                ))
            }
        };
        let recipients = if !input.recipient_ids.is_empty() {
            input
                .recipient_ids
                .iter()
                .map(|value| normalize_recipient(value))
                .collect::<BridgeResult<Vec<_>>>()?
        } else if !current.recipients.is_empty() {
            current.recipients.clone()
        } else {
            self.conversation_recipients(current.conversation_id)?
        };
        let update = ComposeDraftUpdate {
            text: input.text.clone(),
            recipients,
            attachment_ids: self.check_attachments(&input.attachment_ids)?,
            route,
        };
        let saved = self
            .client
            .save_compose_draft(draft_id, expected, update)
            .map_err(core_error)?;
        Ok(dto::draft_view(&saved))
    }

    /// Sends the STORED draft at `expectedRevision` through the exact selected, currently
    /// reported gateway/SIM route. Queueing and clearing are one core transaction; `accepted`
    /// means durably queued in the local encrypted outbox, nothing more.
    pub fn send_draft(&self, input: &DraftInput) -> BridgeResult<SendResultView> {
        let draft_id = DraftId::from_str(&input.id)
            .map_err(|_| invalid_draft("Save the draft before sending it."))?;
        let expected = parse_revision(&input.expected_revision)?;
        let (gateway_id, sim_id) = match (input.gateway_id.as_deref(), input.sim_id.as_deref()) {
            (Some(gateway), Some(sim)) if !gateway.is_empty() && !sim.is_empty() => (gateway, sim),
            _ => {
                return Err(BridgeError::new(
                    "invalid-route",
                    "Select a gateway and SIM before sending.",
                ))
            }
        };
        let stored: ComposeDraft = self
            .client
            .compose_draft(draft_id)
            .map_err(core_error)?
            .ok_or_else(|| core_error(openpush_client_core::Error::NotFound))?;
        // Same binding check as `save_draft`, before anything is persisted: a window scoped to
        // one conversation cannot send another conversation's draft by naming its draft ID.
        if !input.conversation_id.is_empty()
            && input.conversation_id != stored.conversation_id.to_string()
        {
            return Err(invalid_draft(
                "The draft does not belong to this conversation.",
            ));
        }
        if stored.revision != expected {
            return Err(core_error(openpush_client_core::Error::StaleDraft {
                current_revision: stored.revision,
            }));
        }
        let (gateways, known) = self.gateways();
        if !known {
            return Err(BridgeError::new("gateways-unknown", "Gateway capabilities have not been loaded from the server yet; the draft was kept."));
        }
        let gateway = find_route(&gateways, gateway_id, sim_id)
            .ok_or_else(|| BridgeError::new("gateway-unavailable", "The selected gateway/SIM is not currently reported by the server; the draft was kept."))?;
        if !gateway.supports_sms {
            return Err(BridgeError::new(
                "gateway-unsupported",
                "The selected gateway/SIM cannot send SMS; the draft was kept.",
            ));
        }
        if !stored.attachment_ids.is_empty() && !gateway.supports_mms {
            return Err(BridgeError::new(
                "mms-unsupported",
                "The selected gateway/SIM cannot send MMS attachments; the draft was kept.",
            ));
        }
        let route = parse_route(gateway_id, sim_id)?;
        let recipients = if stored.recipients.is_empty() {
            self.conversation_recipients(stored.conversation_id)?
        } else {
            stored.recipients.clone()
        };
        if recipients.is_empty() {
            return Err(BridgeError::new(
                "invalid-recipient",
                "Add a recipient before sending; the draft was kept.",
            ));
        }
        if stored.attachment_ids.is_empty() && recipients.len() != 1 {
            return Err(BridgeError::new(
                "invalid-recipient",
                "An SMS goes to exactly one recipient; group messages need MMS.",
            ));
        }
        if stored.text.trim().is_empty() && stored.attachment_ids.is_empty() {
            return Err(BridgeError::new(
                "empty-message",
                "Write a message or add an attachment before sending.",
            ));
        }
        // Everything core would reject is validated before the route is persisted, so a refused
        // send never bumps the stored revision behind the UI's back.
        if stored.text.len() > MAX_BODY_BYTES {
            return Err(BridgeError::new(
                "message-too-long",
                "The message is too long to send.",
            ));
        }
        if recipients.len() > MAX_RECIPIENTS
            || recipients.iter().any(|r| r.is_empty() || r.len() > 256)
        {
            return Err(BridgeError::new(
                "invalid-recipient",
                "The recipients are not valid for sending.",
            ));
        }
        let attachment_ids: Vec<String> = stored
            .attachment_ids
            .iter()
            .map(ToString::to_string)
            .collect();
        self.check_attachments(&attachment_ids)?;
        // Persist the selected route (and derived recipients) with CAS; content stays as stored.
        let revision = if stored.route.as_ref() != Some(&route) || recipients != stored.recipients {
            let update = ComposeDraftUpdate {
                text: stored.text.clone(),
                recipients,
                attachment_ids: stored.attachment_ids.clone(),
                route: Some(route),
            };
            self.client
                .save_compose_draft(draft_id, expected, update)
                .map_err(core_error)?
                .revision
        } else {
            expected
        };
        if let Err(error) = self.client.send_compose_draft(draft_id, revision) {
            // Report the stored revision so the UI can keep editing without a stale conflict.
            let stored_now = self
                .client
                .compose_draft(draft_id)
                .ok()
                .flatten()
                .map(|d| d.revision)
                .unwrap_or(revision);
            return Err(core_error(error).with_revision(stored_now));
        }
        self.request_work();
        Ok(SendResultView {
            accepted: true,
            status: "queued-local",
            reason: None,
            revision: Some((revision + 1).to_string()),
        })
    }

    pub fn mark_seen(&self, ids: &[String]) -> BridgeResult<()> {
        if ids.len() > MAX_SEEN_IDS {
            return Err(BridgeError::new(
                "invalid-message",
                "Too many message IDs in one request.",
            ));
        }
        let mut changed = false;
        for id in ids {
            let id = MessageId::from_str(id)
                .map_err(|_| BridgeError::new("invalid-message", "The message ID is invalid."))?;
            changed |= self.client.mark_seen(id).map_err(core_error)?;
        }
        if changed {
            self.request_work();
        }
        Ok(())
    }

    /// Encrypts a natively picked file into core-owned storage; JS receives a sanitized handle.
    pub fn prepare_attachment(&self, path: &Path) -> BridgeResult<AttachmentView> {
        let metadata = fs::metadata(path).map_err(|_| {
            BridgeError::new("attachment-local", "The selected file could not be read.")
        })?;
        if !metadata.is_file() {
            return Err(BridgeError::new(
                "attachment-local",
                "Select a regular file.",
            ));
        }
        let prefix = media::read_prefix(path).map_err(|_| {
            BridgeError::new("attachment-local", "The selected file could not be read.")
        })?;
        let media_type = media::sniff_media_type(&prefix);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".into());
        let info = self
            .client
            .prepare_attachment(path, media_type, &name)
            .map_err(core_error)?;
        let preview = media::is_previewable(media_type)
            .then(|| media::read_capped(path).and_then(|bytes| media::preview_data_url(&bytes)))
            .flatten();
        self.cache_preview(info.attachment_id, preview.clone());
        Ok(dto::attachment_view(&info, None, preview))
    }

    fn cache_preview(&self, id: AttachmentId, preview: Option<String>) {
        let mut cache = self
            .previews
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if cache.len() >= MAX_PREVIEW_CACHE {
            cache.clear();
        }
        cache.insert(id, preview);
    }

    fn attachment_views(
        &self,
        message: &Message,
        budget: &mut usize,
        deferred: &mut bool,
    ) -> Vec<AttachmentView> {
        let errors = self
            .transfer_errors
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        message
            .payload
            .record
            .attachments
            .iter()
            .map(|reference| {
                let id = reference.attachment_id;
                let Ok(info) = self.client.attachment_info(id) else {
                    return AttachmentView {
                        id: id.to_string(),
                        name: "Attachment".into(),
                        media_type: "application/octet-stream".into(),
                        byte_size: 0,
                        state: "pending",
                        error: None,
                        preview_url: None,
                    };
                };
                let mut preview = None;
                if media::is_previewable(&info.media_type) && info.state.is_local() {
                    let cached = self
                        .previews
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .get(&id)
                        .cloned();
                    preview = match cached {
                        Some(value) => value,
                        None if *budget > 0 => {
                            *budget -= 1;
                            let value = self
                                .client
                                .open_native_plaintext(id)
                                .ok()
                                .and_then(|file| media::read_capped(file.path()))
                                .and_then(|bytes| media::preview_data_url(&bytes));
                            self.cache_preview(id, value.clone());
                            value
                        }
                        None => {
                            *deferred = true;
                            None
                        }
                    };
                }
                dto::attachment_view(&info, errors.get(&id).cloned(), preview)
            })
            .collect()
    }

    /// Builds the sanitized UI snapshot from core state. Returns whether previews were deferred
    /// (the caller then emits another state hint).
    pub fn snapshot(
        &self,
        requested: Option<&str>,
        head: Head,
        origin: Option<String>,
        notification_preferences: NotificationPreferences,
    ) -> BridgeResult<(Snapshot, bool)> {
        let conversations = self.client.list_conversations().map_err(core_error)?;
        let drafts = self.client.compose_drafts().map_err(core_error)?;
        let known: HashSet<ConversationId> = conversations
            .iter()
            .map(|c| c.conversation_id)
            .chain(drafts.iter().map(|d| d.conversation_id))
            .collect();
        let requested = requested
            .and_then(|id| ConversationId::from_str(id).ok())
            .filter(|id| known.contains(id));
        let active = requested
            .or_else(|| conversations.first().map(|c| c.conversation_id))
            .or_else(|| drafts.first().map(|d| d.conversation_id));
        let (mut budget, mut deferred) = (MAX_PREVIEWS_PER_SNAPSHOT, false);
        let mut views = Vec::new();
        for conversation in &conversations {
            let messages = self
                .client
                .messages(conversation.conversation_id)
                .map_err(core_error)?;
            let last = messages.last();
            let draft = drafts
                .iter()
                .find(|d| d.conversation_id == conversation.conversation_id);
            let name = last
                .map(dto::counterpart)
                .or_else(|| draft.map(|d| d.recipients.join(", ")))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Conversation".into());
            let preview = last
                .map(|m| {
                    if m.payload.body.is_empty() && !m.payload.record.attachments.is_empty() {
                        "Attachment".into()
                    } else {
                        m.payload.body.clone()
                    }
                })
                .unwrap_or_default();
            let message_views = if Some(conversation.conversation_id) == active {
                let start = messages.len().saturating_sub(MAX_ACTIVE_MESSAGES);
                messages[start..]
                    .iter()
                    .map(|m| {
                        dto::message_view(m, self.attachment_views(m, &mut budget, &mut deferred))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            views.push(ConversationView {
                id: conversation.conversation_id.to_string(),
                name,
                preview,
                unread: conversation.unread_count,
                messages: message_views,
            });
        }
        let listed: HashSet<ConversationId> =
            conversations.iter().map(|c| c.conversation_id).collect();
        for draft in drafts
            .iter()
            .filter(|d| !listed.contains(&d.conversation_id))
        {
            views.push(ConversationView {
                id: draft.conversation_id.to_string(),
                name: if draft.recipients.is_empty() {
                    "New message".into()
                } else {
                    draft.recipients.join(", ")
                },
                preview: if draft.text.is_empty() {
                    "Draft".into()
                } else {
                    format!("Draft: {}", draft.text.chars().take(80).collect::<String>())
                },
                unread: 0,
                messages: Vec::new(),
            });
        }
        let draft = active
            .and_then(|id| drafts.iter().find(|d| d.conversation_id == id))
            .map(dto::draft_view);
        let pending = self
            .client
            .pending_outbox_batch(PENDING_COUNT_CAP)
            .map_err(core_error)?
            .len() as u64;
        let quarantine = self.client.quarantined().map_err(core_error)?.len() as u64;
        let notification_snapshot = self.client.notification_snapshot().map_err(core_error)?;
        let keys = self.client.key_status().map_err(core_error)?;
        let status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let unlocked = keys
            .active_epoch
            .is_some_and(|epoch| keys.unlocked_epochs.contains(&epoch));
        let behind = matches!((&status.vault, keys.active_epoch), (Some(vault), Some(active)) if vault.epoch > active);
        let encryption = if self.mismatch.load(Ordering::Relaxed) {
            dto::Encryption {
                state: "mismatch",
                profile_fingerprint: None,
            }
        } else if unlocked && !behind {
            dto::Encryption {
                state: "unlocked",
                profile_fingerprint: status
                    .vault
                    .as_ref()
                    .filter(|v| Some(v.epoch) == keys.active_epoch)
                    .map(|v| v.fingerprint.clone()),
            }
        } else {
            dto::Encryption {
                state: "locked",
                profile_fingerprint: None,
            }
        };
        let connection = if status.revoked {
            dto::Connection {
                state: "error",
                origin,
                error_code: Some("revoked"),
            }
        } else if status.live {
            dto::Connection {
                state: "connected",
                origin,
                error_code: status.work_error,
            }
        } else {
            dto::Connection {
                state: "offline",
                origin,
                error_code: status.error_code.or(Some("connecting")),
            }
        };
        let snapshot = Snapshot {
            version: "1",
            mode: "native",
            connection,
            encryption,
            gateways: status.gateways.clone(),
            conversations: views,
            notifications: notification_snapshot.notifications,
            app_filters: notification_snapshot.app_filters,
            notification_preferences,
            active_conversation_id: active.map(|id| id.to_string()),
            draft,
            head,
            pending_count: pending,
            quarantine_count: quarantine,
        };
        Ok((snapshot, deferred))
    }

    /// Persists the last reported gateway views (non-secret display/routing data, mode 0600).
    pub fn persist_gateways(&self, views: &[GatewayView]) {
        if let Ok(bytes) = serde_json::to_vec(views) {
            let _ = crate::fsutil::write_private_atomic(&self.data_dir.join(GATEWAYS_FILE), &bytes);
        }
    }

    pub fn set_status(&self, update: impl FnOnce(&mut NetStatus)) {
        update(
            &mut self
                .status
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
    }
}
