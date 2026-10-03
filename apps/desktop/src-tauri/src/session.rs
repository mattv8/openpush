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
use peppy_client_core::{
    AttachmentId, Client, ClientConfig, ComposeDraft, ComposeDraftUpdate, ConversationId,
    DatabaseKey, DeviceId, Direction, DraftId, GatewayRoute, Message, MessageId, NativeKeyCache,
    Transport, VaultId,
};
use serde::{Deserialize, Serialize};
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
const FALLBACK_MMS_MAX_BYTES: u64 = 300 * 1024;
pub const GATEWAYS_FILE: &str = "gateways.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultSummary {
    pub epoch: u32,
    pub fingerprint: String,
}

/// Sanitized pairing state for the webview. Owner credentials and the approval challenge stay
/// in the native session.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingIntentView {
    pub https_origin: String,
    pub intent_token: String,
    pub expires_in_seconds: i64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingStatusView {
    pub claimed: bool,
    pub approved: bool,
    pub device_id: Option<String>,
    pub key_digest: Option<String>,
    pub sas: Option<String>,
    pub expires_in_seconds: i64,
}

#[derive(Deserialize)]
struct PairingIntentResponse {
    https_origin: String,
    intent_token: String,
    expires_in_seconds: i64,
}

#[derive(Deserialize)]
struct PairingStatusResponse {
    claimed: bool,
    approved: bool,
    device_id: Option<String>,
    key_digest: Option<String>,
    sas: Option<String>,
    expires_in_seconds: i64,
}

#[derive(Deserialize)]
struct PairingApprovalResponse {}

fn pairing_token(value: &str) -> BridgeResult<()> {
    (value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
    .then_some(())
    .ok_or_else(|| BridgeError::new("invalid-pairing-intent", "The pairing request is invalid."))
}

fn pairing_owner_error(error: crate::net::NetError) -> BridgeError {
    match error {
        crate::net::NetError::Status {
            status: 403,
            code: Some(code),
        } if code == "owner_required" => BridgeError::new(
            "pairing-owner-required",
            "This desktop device is not an owner, so it cannot pair a phone.",
        ),
        other => other.into(),
    }
}

fn pairing_status_error() -> BridgeError {
    BridgeError::new(
        "invalid-pairing-status",
        "The server returned an invalid pairing verification state.",
    )
}

fn valid_pairing_digest(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| {
            byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte.is_ascii_hexdigit())
        })
}

fn valid_pairing_sas(value: &str) -> bool {
    value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn validated_pairing_status(
    intent_token: &str,
    response: PairingStatusResponse,
) -> BridgeResult<PairingStatusView> {
    let PairingStatusResponse {
        claimed,
        approved,
        device_id,
        key_digest,
        sas,
        expires_in_seconds,
    } = response;
    match (claimed, device_id, key_digest, sas) {
        (false, None, None, None) => Ok(PairingStatusView {
            claimed: false,
            approved: false,
            device_id: None,
            key_digest: None,
            sas: None,
            expires_in_seconds: expires_in_seconds.max(0),
        }),
        (true, Some(device_id), Some(key_digest), Some(server_sas))
            if valid_pairing_digest(&key_digest) && valid_pairing_sas(&server_sas) =>
        {
            let device = DeviceId::from_str(&device_id).map_err(|_| pairing_status_error())?;
            let expected = peppy_protocol::pairing_sas(intent_token, &key_digest, device);
            if expected != server_sas {
                return Err(pairing_status_error());
            }
            Ok(PairingStatusView {
                claimed: true,
                approved,
                device_id: Some(device_id),
                key_digest: Some(key_digest),
                sas: Some(expected),
                expires_in_seconds: expires_in_seconds.max(0),
            })
        }
        _ => Err(pairing_status_error()),
    }
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
    /// Wakes the live loop to run a requested contact-projection repair snapshot now.
    pub repair_wake: Notify,
    /// One fenced repair snapshot per session (or per manual request) while the latch is set.
    pub repair_attempted: AtomicBool,
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
                    peppy_client_core::Error::InvalidKeyCache
                    | peppy_client_core::Error::InvalidProfile,
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
        .and_then(|b| serde_json::from_slice(&b).ok())
        .map(|mut gateways: Vec<GatewayView>| {
            for gateway in &mut gateways {
                gateway.supports_mms &= gateway.mms_content_version.unwrap_or(0) >= 2;
            }
            gateways
        });
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
        repair_wake: Notify::new(),
        repair_attempted: AtomicBool::new(false),
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

    pub async fn create_pairing_intent(&self) -> BridgeResult<PairingIntentView> {
        let response: PairingIntentResponse = self
            .api
            .post_json(
                "/v1/pairing/intents",
                &serde_json::json!({ "https_origin": self.api.origin() }),
            )
            .await
            .map_err(pairing_owner_error)?;
        pairing_token(&response.intent_token)?;
        Ok(PairingIntentView {
            https_origin: response.https_origin,
            intent_token: response.intent_token,
            expires_in_seconds: response.expires_in_seconds,
        })
    }

    pub async fn pairing_intent_status(
        &self,
        intent_token: &str,
    ) -> BridgeResult<PairingStatusView> {
        pairing_token(intent_token)?;
        let response: PairingStatusResponse = self
            .api
            .get_json(
                &format!("/v1/pairing/intents/{intent_token}"),
                crate::net::MAX_JSON_BYTES,
            )
            .await
            .map_err(pairing_owner_error)?;
        validated_pairing_status(intent_token, response)
    }

    pub async fn approve_pairing_intent(
        &self,
        intent_token: &str,
        key_digest: &str,
    ) -> BridgeResult<()> {
        pairing_token(intent_token)?;
        if !valid_pairing_digest(key_digest) {
            return Err(BridgeError::new(
                "invalid-pairing-key",
                "The phone verification key is invalid.",
            ));
        }
        let status = self.pairing_intent_status(intent_token).await?;
        if !status.claimed || status.approved || status.key_digest.as_deref() != Some(key_digest) {
            return Err(BridgeError::new(
                "pairing-claim-changed",
                "The claimed phone changed or is no longer awaiting approval.",
            ));
        }
        let vault =
            crate::sync::fetch_vault(&self.api, &self.binding.vault_id, &self.binding.device_id)
                .await?;
        let (profile, _) = crate::sync::vault_header(&vault)?;
        let _: PairingApprovalResponse = self
            .api
            .post_json(
                &format!("/v1/pairing/intents/{intent_token}/approve"),
                &serde_json::json!({
                    "key_digest": key_digest,
                    "profile_fingerprint": vault.profile_fingerprint,
                    "key_epoch": profile.key_epoch,
                }),
            )
            .await
            .map_err(pairing_owner_error)?;
        Ok(())
    }

    /// Reply recipients and transport derived from the latest stored message only.
    fn conversation_context(
        &self,
        conversation: ConversationId,
    ) -> BridgeResult<(Vec<String>, Option<Transport>)> {
        let messages = self.client.messages(conversation).map_err(core_error)?;
        let Some(latest) = messages.last() else {
            return Ok((Vec::new(), None));
        };
        let transport = latest.payload.transport;
        if transport == Transport::Mms {
            let context = self
                .client
                .mms_reply_context(conversation)
                .map_err(core_error)?;
            if let Some(reason) = context.blocked_reason {
                return Err(BridgeError::new("mms-reply-blocked", reason));
            }
            return Ok((context.recipients, Some(transport)));
        }
        Ok((other_party(latest), Some(transport)))
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
            .ok_or_else(|| core_error(peppy_client_core::Error::NotFound))?;
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
        let recipients = input
            .recipient_ids
            .iter()
            .map(|value| normalize_recipient(value))
            .collect::<BridgeResult<Vec<_>>>()?;
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
            .ok_or_else(|| core_error(peppy_client_core::Error::NotFound))?;
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
            return Err(core_error(peppy_client_core::Error::StaleDraft {
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
        let route = parse_route(gateway_id, sim_id)?;
        let (derived_recipients, latest_transport) =
            self.conversation_context(stored.conversation_id)?;
        let recipients = if stored.recipients.is_empty() {
            derived_recipients
        } else {
            stored.recipients.clone()
        };
        if recipients.is_empty() {
            return Err(BridgeError::new(
                "invalid-recipient",
                "Add a recipient before sending; the draft was kept.",
            ));
        }
        if latest_transport == Some(Transport::Mms) {
            let context = self
                .client
                .mms_reply_context(stored.conversation_id)
                .map_err(core_error)?;
            if let Some(reason) = context.blocked_reason {
                return Err(BridgeError::new("mms-reply-blocked", reason));
            }
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
        let attachment_info: Vec<_> = attachment_ids
            .iter()
            .map(|id| {
                let id = AttachmentId::from_str(id).map_err(|_| {
                    BridgeError::new("invalid-attachment", "The attachment ID is invalid.")
                })?;
                self.client.attachment_info(id).map_err(core_error)
            })
            .collect::<BridgeResult<_>>()?;
        self.check_attachments(&attachment_ids)?;
        let requires_mms = !attachment_info.is_empty()
            || recipients.len() > 1
            || latest_transport == Some(Transport::Mms);
        if requires_mms {
            if !gateway.supports_mms || gateway.mms_content_version.unwrap_or(0) < 2 {
                return Err(BridgeError::new(
                    "mms-unsupported",
                    "The selected gateway/SIM does not report MMS capability version 2; the draft was kept.",
                ));
            }
            let max_recipients = gateway.mms_max_recipients.unwrap_or(MAX_RECIPIENTS);
            if recipients.len() > max_recipients {
                return Err(BridgeError::new(
                    "mms-too-many-recipients",
                    "This MMS route does not support that many recipients; the draft was kept.",
                ));
            }
            // This deliberately excludes PDU headers and encoding overhead. It can refuse a
            // known-too-large draft but never claims that a lower bound will fit the carrier PDU.
            let attachment_bytes = attachment_info.iter().try_fold(0u64, |total, info| {
                total.checked_add(info.plaintext_bytes).ok_or_else(|| {
                    BridgeError::new(
                        "mms-too-large",
                        "The MMS is too large to send; the draft was kept.",
                    )
                })
            })?;
            let lower_bound = attachment_bytes
                .checked_add(stored.text.len() as u64)
                .ok_or_else(|| {
                    BridgeError::new(
                        "mms-too-large",
                        "The MMS is too large to send; the draft was kept.",
                    )
                })?;
            if lower_bound > gateway.mms_max_bytes.unwrap_or(FALLBACK_MMS_MAX_BYTES) {
                return Err(BridgeError::new(
                    "mms-too-large",
                    "The MMS content already exceeds this route's size limit; the draft was kept.",
                ));
            }
        }
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
        let expected_transport = if requires_mms {
            Transport::Mms
        } else {
            Transport::Sms
        };
        if let Err(error) =
            self.client
                .send_compose_draft_checked_transport(draft_id, revision, expected_transport)
        {
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
        self.mark_seen_scoped(ids, None)
    }

    /// Validates a complete batch before applying any read marks. Composer
    /// ownership loads its transcript exactly once, so mixed-conversation IDs
    /// cannot partially acknowledge messages before the scope error is found.
    pub fn mark_seen_scoped(
        &self,
        ids: &[String],
        conversation: Option<ConversationId>,
    ) -> BridgeResult<()> {
        if ids.len() > MAX_SEEN_IDS {
            return Err(BridgeError::new(
                "invalid-message",
                "Too many message IDs in one request.",
            ));
        }
        let parsed = ids
            .iter()
            .map(|id| {
                MessageId::from_str(id)
                    .map_err(|_| BridgeError::new("invalid-message", "The message ID is invalid."))
            })
            .collect::<BridgeResult<Vec<_>>>()?;
        if let Some(conversation) = conversation {
            let allowed = self
                .client
                .messages(conversation)
                .map_err(core_error)?
                .into_iter()
                .map(|message| message.payload.record.message_id)
                .collect::<HashSet<_>>();
            if parsed.iter().any(|id| !allowed.contains(id)) {
                return Err(BridgeError::new(
                    "window-context",
                    "A composer window can only mark its own conversation messages as seen.",
                ));
            }
        }
        let mut changed = false;
        for id in parsed {
            changed |= self.client.mark_seen(id).map_err(core_error)?;
        }
        if changed {
            self.notify();
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

    /// Retries only a locally recorded transfer failure. It never changes message state or queues
    /// a carrier command; the normal worker owns both of those durable transitions.
    pub fn retry_attachment(&self, id: &str) -> BridgeResult<()> {
        let id = AttachmentId::from_str(id)
            .map_err(|_| BridgeError::new("invalid-attachment", "The attachment ID is invalid."))?;
        self.client.attachment_info(id).map_err(core_error)?;
        let removed = self
            .transfer_errors
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id)
            .is_some();
        if !removed {
            return Err(BridgeError::new(
                "attachment-not-retryable",
                "This attachment has no retryable transfer failure.",
            ));
        }
        self.request_work();
        Ok(())
    }

    pub fn attachment_in_conversation(
        &self,
        id: &str,
        conversation: ConversationId,
    ) -> BridgeResult<bool> {
        let id = AttachmentId::from_str(id)
            .map_err(|_| BridgeError::new("invalid-attachment", "The attachment ID is invalid."))?;
        Ok(self
            .client
            .messages(conversation)
            .map_err(core_error)?
            .iter()
            .any(|message| {
                message
                    .payload
                    .record
                    .attachments
                    .iter()
                    .any(|reference| reference.attachment_id == id)
            }))
    }

    pub fn save_attachment(&self, id: &str, destination: &Path) -> BridgeResult<()> {
        let id = AttachmentId::from_str(id)
            .map_err(|_| BridgeError::new("invalid-attachment", "The attachment ID is invalid."))?;
        let info = self.client.attachment_info(id).map_err(core_error)?;
        if !info.state.is_local() {
            return Err(BridgeError::new(
                "attachment-unavailable",
                "This attachment is not verified and ready to save.",
            ));
        }
        let plaintext = self.client.open_native_plaintext(id).map_err(core_error)?;
        crate::fsutil::copy_new_atomic(plaintext.path(), destination, info.plaintext_bytes).map_err(
            |_| {
                BridgeError::new(
                    "attachment-save",
                    "The attachment could not be saved safely.",
                )
            },
        )
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
                        transfer: Some("download"),
                        retryable: None,
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
            let reply_context = self
                .client
                .mms_reply_context(conversation.conversation_id)
                .ok();
            views.push(ConversationView {
                id: conversation.conversation_id.to_string(),
                name,
                preview,
                unread: conversation.unread_count,
                messages: message_views,
                participants: reply_context
                    .as_ref()
                    .map(|context| context.recipients.clone()),
                reply_blocked_reason: reply_context.and_then(|context| context.blocked_reason),
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
                participants: None,
                reply_blocked_reason: None,
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
        drop(status);
        let mut addresses: Vec<(String, Option<String>)> = Vec::new();
        for view in &views {
            for address in view.participants.iter().flatten().chain([&view.name]) {
                addresses.push((address.clone(), None));
            }
        }
        for draft in &drafts {
            let source = draft
                .route
                .as_ref()
                .map(|r| r.gateway_device_id.to_string());
            for recipient in &draft.recipients {
                addresses.push((recipient.clone(), source.clone()));
            }
        }
        for notification in &notification_snapshot.notifications {
            addresses.push((
                notification.title.clone(),
                Some(notification.target.source_device_id.clone()),
            ));
        }
        let contacts = crate::contacts::snapshot_contacts(self, &addresses);
        let status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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
            desktop: None,
            pending_count: pending,
            quarantine_count: quarantine,
            contact_resolution: contacts.resolution,
            contact_books: contacts.books,
            contacts_pending_count: contacts.pending_count,
            contact_sync: contacts.sync,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{fixture_at, PHRASE};
    use peppy_client_core::IncomingSms;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn pairing_intent_tokens_are_canonical_base64url() {
        assert!(pairing_token(&"A".repeat(43)).is_ok());
        assert!(pairing_token("short").is_err());
        assert!(pairing_token(&format!("{}+", "A".repeat(42))).is_err());
    }

    #[test]
    fn pairing_status_recomputes_the_shared_sas_vector() {
        let view = validated_pairing_status(
            "intent-1",
            PairingStatusResponse {
                claimed: true,
                approved: false,
                device_id: Some("00000000-0000-0000-0000-000000000002".into()),
                key_digest: Some(
                    "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0".into(),
                ),
                sas: Some("122032".into()),
                expires_in_seconds: 300,
            },
        )
        .unwrap();
        assert_eq!(view.sas.as_deref(), Some("122032"));
        assert_eq!(
            view.device_id.as_deref(),
            Some("00000000-0000-0000-0000-000000000002")
        );
    }

    #[test]
    fn pairing_status_rejects_a_server_sas_that_does_not_match() {
        assert!(validated_pairing_status(
            "intent-1",
            PairingStatusResponse {
                claimed: true,
                approved: false,
                device_id: Some("00000000-0000-0000-0000-000000000002".into()),
                key_digest: Some(
                    "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0".into()
                ),
                sas: Some("000000".into()),
                expires_in_seconds: 300,
            },
        )
        .is_err());
    }

    #[test]
    fn mark_seen_scoped_notifies_once_when_messages_change() {
        let fixture = fixture_at("http://127.0.0.1:9");
        let count = Arc::new(AtomicUsize::new(0));
        let notifier_count = count.clone();
        let session = open_session(
            fixture.dir.path(),
            &fixture.store,
            &fixture.binding,
            &[],
            Arc::new(move || {
                notifier_count.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .unwrap();
        session
            .client
            .unlock(&fixture.profile, &fixture.header, PHRASE)
            .unwrap();
        let draft = session
            .save_draft(
                &serde_json::from_value(serde_json::json!({
                    "id": "mark-seen-draft", "conversationId": "", "text": "",
                    "recipientIds": ["+15555550100"], "attachmentIds": [], "expectedRevision": "0"
                }))
                .unwrap(),
            )
            .unwrap();
        let conversation = ConversationId::from_str(&draft.conversation_id).unwrap();
        let captured = session
            .client
            .capture_incoming(IncomingSms {
                conversation_id: Some(conversation),
                sender_address: "+15555550100".into(),
                body: "unread".into(),
                provider_message_id: Some("mark-seen-notify".into()),
                imported: false,
            })
            .unwrap();
        assert_eq!(captured.conversation_id, conversation);
        let id = session.client.messages(conversation).unwrap()[0]
            .payload
            .record
            .message_id
            .to_string();

        session.mark_seen_scoped(&[id], Some(conversation)).unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }
}
