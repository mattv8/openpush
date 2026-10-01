//! OpenPush desktop native host. The webview only receives sanitized view models (`dto`) and
//! state-change hints; credentials, keys, passphrases, tokens and file paths stay in Rust.
use std::{
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tauri::{Emitter, Manager, RunEvent, State, WindowEvent};
#[cfg(feature = "native-head-probe")]
use uuid::Uuid;

mod credentials;
mod dialogs;
mod dto;
mod error;
mod fsutil;
mod gateways;
mod media;
mod net;
mod origin;
mod secure_store;
#[cfg(test)]
mod security_tests;
mod session;
mod sync;
#[cfg(test)]
mod tests;
mod tray;
mod windows;

use credentials::{
    check_origin_binding, ensure_database_key, load_config, parse_credential, read_credential_file,
    save_config, store_credential, HostConfig,
};
use dto::{DraftView, Head, PublicCopyView, SendResultView, Snapshot};
use error::{core_error, BridgeError, BridgeResult};
use openpush_client_core::{AttachmentId, ConversationId};
use secure_store::{KeyringStore, SecretStore};
use session::{open_session, DraftInput, Notifier, Session, VaultSummary};
use sync::{blocking, fetch_vault, vault_header};

pub const STATE_EVENT: &str = "openpush://state";
static TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct AppState {
    root: PathBuf,
    config_path: PathBuf,
    store: Arc<dyn SecretStore>,
    config_lock: Mutex<()>,
    session: tokio::sync::Mutex<Option<Arc<Session>>>,
    /// Serializes credential import: origin check, key initialization, credential storage,
    /// config activation and session replacement happen as one unit.
    import_lock: tokio::sync::Mutex<()>,
    notifier: Notifier,
}

/// Commands a composer window may call (mirrors `capabilities/composer.json`).
pub const COMPOSER_COMMANDS: &[&str] = &[
    "load_state",
    "save_draft",
    "send_draft",
    "mark_seen",
    "pick_attachments",
    "close_composer",
];

fn window_error() -> BridgeError {
    BridgeError::new(
        "window-context",
        "This operation is only available in the main window.",
    )
}

/// Runtime defense in depth behind the Tauri ACL: settings, import, unlock, publication,
/// composer creation and head commands are main-window only.
pub fn require_main(label: &str) -> BridgeResult<()> {
    if label == tray::MAIN {
        Ok(())
    } else {
        Err(window_error())
    }
}

/// A composer window is scoped to the conversation in its label; the main window is unscoped.
pub fn check_conversation_scope(label: &str, conversation: Option<&str>) -> BridgeResult<()> {
    if label == tray::MAIN {
        return Ok(());
    }
    let scope = tray::composer_conversation(label).ok_or_else(window_error)?;
    match conversation.map(ConversationId::from_str) {
        Some(Ok(id)) if id == scope => Ok(()),
        _ => Err(BridgeError::new(
            "window-context",
            "A composer window can only access its own conversation.",
        )),
    }
}

impl AppState {
    pub fn new(root: PathBuf, store: Arc<dyn SecretStore>, notifier: Notifier) -> Self {
        Self {
            config_path: root.join("server.json"),
            root,
            store,
            config_lock: Mutex::new(()),
            session: tokio::sync::Mutex::new(None),
            import_lock: tokio::sync::Mutex::new(()),
            notifier,
        }
    }

    fn config(&self) -> BridgeResult<HostConfig> {
        load_config(&self.config_path)
    }

    fn update_config(&self, update: impl FnOnce(&mut HostConfig)) -> BridgeResult<HostConfig> {
        let _guard = self
            .config_lock
            .lock()
            .map_err(|_| BridgeError::host_state())?;
        let mut config = load_config(&self.config_path)?;
        update(&mut config);
        save_config(&self.config_path, &config)?;
        Ok(config)
    }

    async fn close_session(&self) {
        if let Some(session) = self.session.lock().await.take() {
            session.cancel.cancel();
        }
    }

    /// The session for the active binding, opened on demand. A session for any other binding is
    /// stopped first, so a client/key opened for one credential is never reused for another.
    pub async fn session(&self) -> BridgeResult<Option<Arc<Session>>> {
        let config = self.config()?;
        let mut guard = self.session.lock().await;
        let Some(binding) = config.active_binding().cloned() else {
            if let Some(old) = guard.take() {
                old.cancel.cancel();
            }
            return Ok(None);
        };
        if let Some(existing) = guard.as_ref() {
            if existing.binding == binding {
                return Ok(Some(existing.clone()));
            }
            existing.cancel.cancel();
            *guard = None;
        }
        let epochs = config
            .known(&binding)
            .map(|known| known.cached_epochs.clone())
            .unwrap_or_default();
        let (root, store, notifier) =
            (self.root.clone(), self.store.clone(), self.notifier.clone());
        let session = Arc::new(
            blocking(move || open_session(&root, &*store, &binding, &epochs, notifier)).await?,
        );
        sync::start(&session);
        *guard = Some(session.clone());
        Ok(Some(session))
    }

    async fn require_session(&self) -> BridgeResult<Arc<Session>> {
        self.session().await?.ok_or_else(BridgeError::no_session)
    }
}

fn head() -> Head {
    #[cfg(feature = "native-head-probe")]
    return Head {
        enabled: false,
        capability: "unconfirmed",
        note: Some(windows::head_capability()),
    };
    #[cfg(not(feature = "native-head-probe"))]
    Head { enabled: false, capability: "unsupported", note: Some("Native conversation heads are not part of this build; use the main window or a composer window.".into()) }
}

fn empty_snapshot(origin: Option<String>) -> Snapshot {
    let code = if origin.is_some() {
        "credentials-required"
    } else {
        "server-required"
    };
    Snapshot {
        version: "1",
        mode: "native",
        connection: dto::Connection {
            state: "offline",
            origin,
            error_code: Some(code),
        },
        encryption: dto::Encryption {
            state: "locked",
            profile_fingerprint: None,
        },
        gateways: vec![],
        conversations: vec![],
        active_conversation_id: None,
        draft: None,
        head: head(),
        pending_count: 0,
        quarantine_count: 0,
    }
}

#[tauri::command]
async fn load_state(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    conversation_id: Option<String>,
) -> BridgeResult<Snapshot> {
    check_conversation_scope(window.label(), conversation_id.as_deref())?;
    let origin = state.config()?.origin;
    let Some(session) = state.session().await? else {
        return Ok(empty_snapshot(origin));
    };
    let s = session.clone();
    let (snapshot, deferred) =
        blocking(move || s.snapshot(conversation_id.as_deref(), head(), origin)).await?;
    if deferred {
        session.notify();
    }
    Ok(snapshot)
}

#[tauri::command]
async fn configure_server(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    origin: String,
) -> BridgeResult<()> {
    require_main(window.label())?;
    let origin = origin::validate_origin(&origin)?;
    let _import = state.import_lock.lock().await;
    // Credentials bound to another origin are deactivated (kept in secure storage, never sent to
    // the new origin); a binding previously imported for this origin is reactivated.
    state.update_config(|config| config.select_origin(&origin))?;
    state.close_session().await;
    state.session().await?;
    (state.notifier)();
    Ok(())
}

#[tauri::command]
async fn import_credentials(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> BridgeResult<()> {
    require_main(window.label())?;
    let Some(path) = dialogs::pick_file(&app, "Import OpenPush device credential").await? else {
        return Ok(());
    };
    let bytes = blocking(move || read_credential_file(&path)).await?;
    let credential = Arc::new(parse_credential(&bytes)?);
    drop(bytes);
    import_credential(&state, credential).await?;
    (state.notifier)();
    Ok(())
}

/// Shared by the native import command and tests: proves the credential at its own origin,
/// then activates it under the import lock.
async fn import_credential(
    state: &AppState,
    credential: Arc<credentials::ImportedCredential>,
) -> BridgeResult<()> {
    // Refuse before any network use: the token must never be sent to a non-configured origin.
    check_configured_origin(state, &credential)?;
    let (store, check) = (state.store.clone(), credential.clone());
    blocking(move || check_origin_binding(&*store, &check)).await?;
    let api = net::Api::new(&credential.origin, &credential.device_token)?;
    fetch_vault(&api, &credential.vault_id, &credential.device_id).await?;
    activate_import(state, credential).await
}

fn check_configured_origin(
    state: &AppState,
    credential: &credentials::ImportedCredential,
) -> BridgeResult<()> {
    match state.config()?.origin {
        Some(origin) if origin != credential.origin => Err(BridgeError::new(
            "origin-binding",
            "This credential belongs to a different server. Configure that server URL first; credentials are never sent to another origin.",
        )),
        _ => Ok(()),
    }
}

/// Serialized activation of an already verified credential: the origin binding is re-checked,
/// an existing database key is preserved (never replaced), the credential is stored, the config
/// activated, and any session for the binding is replaced so a rotated token takes effect.
async fn activate_import(
    state: &AppState,
    credential: Arc<credentials::ImportedCredential>,
) -> BridgeResult<()> {
    let _import = state.import_lock.lock().await;
    // Re-checked under the lock: the origin may have changed while the credential was verified.
    check_configured_origin(state, &credential)?;
    let binding = credential.binding();
    let (store, root, keyed, stored) = (
        state.store.clone(),
        state.root.clone(),
        binding.clone(),
        credential.clone(),
    );
    blocking(move || {
        check_origin_binding(&*store, &stored)?;
        ensure_database_key(&*store, &keyed, &keyed.database_path(&root))?;
        store_credential(&*store, &stored)
    })
    .await?;
    state.update_config(|config| {
        config.select_origin(&binding.origin);
        config.remember(&binding);
        config.active = Some(binding.clone());
    })?;
    state.close_session().await;
    state.session().await?;
    Ok(())
}

#[tauri::command]
async fn unlock_sync(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> BridgeResult<()> {
    require_main(window.label())?;
    let session = state.require_session().await?;
    let vault = fetch_vault(
        &session.api,
        &session.binding.vault_id,
        &session.binding.device_id,
    )
    .await?;
    let (profile, header) = vault_header(&vault)?;
    let Some(passphrase) = dialogs::passphrase(&app).await? else {
        return Ok(());
    };
    let result = unlock_with(
        &state,
        &session,
        profile,
        header,
        passphrase,
        vault.profile_fingerprint,
    )
    .await;
    session.notify();
    result
}

async fn unlock_with(
    state: &AppState,
    session: &Arc<Session>,
    profile: openpush_client_core::KeyProfile,
    header: openpush_client_core::VaultCheckHeader,
    passphrase: zeroize::Zeroizing<String>,
    fingerprint: String,
) -> BridgeResult<()> {
    let epoch = profile.key_epoch;
    let (s, store) = (session.clone(), state.store.clone());
    blocking(move || {
        match s.client.unlock(&profile, &header, &passphrase) {
            Err(openpush_client_core::Error::InvalidProfile) => {
                s.mismatch.store(true, Ordering::Relaxed);
                return Err(core_error(openpush_client_core::Error::InvalidProfile));
            }
            other => other.map_err(core_error)?,
        }
        s.mismatch.store(false, Ordering::Relaxed);
        // The server reports this epoch as current; local work must be sealed under it.
        if s.client
            .key_status()
            .map_err(core_error)?
            .active_epoch
            .is_some_and(|active| active < epoch)
        {
            s.client.activate_epoch(epoch).map_err(core_error)?;
        }
        let cache = s
            .client
            .export_native_key_cache(epoch)
            .map_err(core_error)?;
        store.set(
            &s.binding.key_cache_account(epoch),
            cache.native_storage_bytes(),
        )
    })
    .await?;
    let binding = session.binding.clone();
    state.update_config(|config| config.add_cached_epoch(&binding, epoch))?;
    session.set_status(|status| status.vault = Some(VaultSummary { epoch, fingerprint }));
    session.request_work();
    session.reconnect_wake.notify_one();
    Ok(())
}

#[tauri::command]
async fn save_draft(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    input: DraftInput,
) -> BridgeResult<DraftView> {
    check_conversation_scope(window.label(), Some(input.conversation_id.as_str()))?;
    let session = state.require_session().await?;
    blocking(move || session.save_draft(&input)).await
}

#[tauri::command]
async fn send_draft(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    input: DraftInput,
) -> BridgeResult<SendResultView> {
    check_conversation_scope(window.label(), Some(input.conversation_id.as_str()))?;
    let session = state.require_session().await?;
    let s = session.clone();
    let result = blocking(move || s.send_draft(&input)).await;
    session.notify();
    result
}

#[tauri::command]
async fn mark_seen(
    state: State<'_, AppState>,
    visible_message_ids: Vec<String>,
) -> BridgeResult<()> {
    let session = state.require_session().await?;
    blocking(move || session.mark_seen(&visible_message_ids)).await
}

#[tauri::command]
async fn pick_attachments(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> BridgeResult<Vec<dto::AttachmentView>> {
    let session = state.require_session().await?;
    let paths = dialogs::pick_files(&app, "Attach files (encrypted before upload)").await?;
    if paths.len() > 10 {
        return Err(BridgeError::new(
            "invalid-attachment",
            "Select at most 10 files.",
        ));
    }
    blocking(move || {
        paths
            .iter()
            .map(|path| session.prepare_attachment(path))
            .collect()
    })
    .await
}

#[derive(serde::Deserialize)]
struct PublicCopyResponse {
    token: String,
    safe_name: String,
    #[serde(default)]
    expires_in_seconds: Option<u64>,
}

#[tauri::command]
async fn publish_attachment(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> BridgeResult<Option<PublicCopyView>> {
    require_main(window.label())?;
    let session = state.require_session().await?;
    let attachment = AttachmentId::from_str(&id)
        .map_err(|_| BridgeError::new("invalid-attachment", "The attachment ID is invalid."))?;
    let s = session.clone();
    let info = blocking(move || s.client.attachment_info(attachment).map_err(core_error)).await?;
    if !media::is_previewable(&info.media_type) || !info.state.is_local() {
        return Err(BridgeError::new(
            "public-copy-unsupported",
            "Only images available on this device can be shared as a public copy.",
        ));
    }
    let remote = session
        .remote_ids
        .lock()
        .map_err(|_| BridgeError::host_state())?
        .get(attachment)
        .ok_or_else(|| {
            BridgeError::new(
                "public-copy-unavailable",
                "This image's server object is not known on this device yet; wait until it has finished sending or downloading.",
            )
        })?;
    // Decode/re-encode locally first so the confirmation names exactly what would be exposed.
    let prepared = prepare_public_copy(&session, attachment, &info.display_name).await?;
    let confirmed = dialogs::confirm(
        &app,
        "Create a public copy?",
        &public_copy_prompt(&info.display_name, &prepared),
        "Create public copy",
    )
    .await?;
    if !confirmed {
        return Ok(None);
    }
    let view = upload_public_copy(&session, &remote, prepared).await?;
    // The URL is shown to the person and returned to the UI; it is never logged.
    dialogs::inform(
        &app,
        "Public copy created",
        &format!("Anyone with this link can view the copy:\n{}", view.url),
    );
    Ok(Some(view))
}

pub struct PreparedPublicCopy {
    bytes: Vec<u8>,
    name: String,
    width: u32,
    height: u32,
}

/// Native confirmation text naming the sanitized image and the derivative that would be public.
pub fn public_copy_prompt(display_name: &str, prepared: &PreparedPublicCopy) -> String {
    let shown: String = display_name
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    format!(
        "Share \"{shown}\" publicly?\n\nOpenPush will upload a SEPARATE, server-readable copy named \"{}\" ({}×{} px, {} KiB, re-encoded with metadata removed). Anyone with the link can view it until it expires or is revoked. The private encrypted original is not changed.",
        prepared.name,
        prepared.width,
        prepared.height,
        prepared.bytes.len().div_ceil(1024)
    )
}

/// Re-encodes the verified local plaintext into a metadata-free derivative (no network).
pub(crate) async fn prepare_public_copy(
    session: &Arc<Session>,
    attachment: AttachmentId,
    display_name: &str,
) -> BridgeResult<PreparedPublicCopy> {
    let s = session.clone();
    let public = blocking(move || {
        let file = s
            .client
            .open_native_plaintext(attachment)
            .map_err(core_error)?;
        let bytes = media::read_capped(file.path()).ok_or_else(|| {
            BridgeError::new(
                "public-copy-too-large",
                "The image is too large for a public copy.",
            )
        })?;
        drop(file);
        media::reencode_public(&bytes)
    })
    .await?;
    Ok(PreparedPublicCopy {
        name: media::public_name(display_name, public.extension),
        width: public.width,
        height: public.height,
        bytes: public.bytes,
    })
}

/// Uploads a prepared derivative. Callers must already hold explicit native user confirmation.
pub(crate) async fn upload_public_copy(
    session: &Arc<Session>,
    remote: &str,
    prepared: PreparedPublicCopy,
) -> BridgeResult<PublicCopyView> {
    let response: PublicCopyResponse = session
        .api
        .post_public_copy(
            &format!("/v1/attachments/{remote}/public-copies"),
            &prepared.name,
            prepared.bytes,
        )
        .await?;
    let safe = |value: &str, extra: &[char]| {
        !value.is_empty()
            && value.len() <= 128
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
    };
    if !safe(&response.token, &['-', '_']) || !safe(&response.safe_name, &['-', '_', '.']) {
        return Err(net::NetError::Invalid.into());
    }
    let url = format!(
        "{}/file/mms-usercontent/{}/{}",
        session.api.origin(),
        response.token,
        response.safe_name
    );
    Ok(PublicCopyView {
        url,
        expires_in_seconds: response.expires_in_seconds.unwrap_or(0),
    })
}

/// Test helper: prepare + upload (confirmation is the caller's responsibility).
#[cfg(test)]
async fn create_public_copy(
    session: &Arc<Session>,
    attachment: AttachmentId,
    display_name: &str,
    remote: &str,
) -> BridgeResult<PublicCopyView> {
    let prepared = prepare_public_copy(session, attachment, display_name).await?;
    upload_public_copy(session, remote, prepared).await
}

async fn new_composer(app: tauri::AppHandle, conversation: Option<String>) -> BridgeResult<()> {
    let state = app.state::<AppState>();
    let conversation = match conversation {
        Some(id) => ConversationId::from_str(&id).map_err(|_| {
            BridgeError::new("invalid-conversation", "The conversation ID is invalid.")
        })?,
        None => {
            let session = state.require_session().await?;
            blocking(move || {
                session
                    .client
                    .create_compose_draft(None)
                    .map_err(core_error)
            })
            .await?
            .conversation_id
        }
    };
    tray::open_composer(&app, conversation)
}

/// Explicit user action only (main window or tray). `None` starts a new conversation draft.
#[tauri::command]
async fn open_composer(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    conversation_id: Option<String>,
) -> BridgeResult<()> {
    require_main(window.label())?;
    new_composer(app, conversation_id).await
}

#[tauri::command]
fn show_head(_conversation_id: String) -> BridgeResult<()> {
    Err(BridgeError::new(
        "heads-unsupported",
        "Native heads are disabled; use the main window or a composer window.",
    ))
}
#[tauri::command]
fn update_head(_conversation_id: String) -> BridgeResult<()> {
    Err(BridgeError::new(
        "heads-unsupported",
        "Native heads are disabled; use the main window or a composer window.",
    ))
}
#[tauri::command]
fn hide_head(_conversation_id: String) -> BridgeResult<()> {
    Err(BridgeError::new(
        "heads-unsupported",
        "Native heads are disabled; use the main window or a composer window.",
    ))
}

#[tauri::command]
fn close_composer(window: tauri::WebviewWindow) -> BridgeResult<()> {
    if tray::composer_conversation(window.label()).is_none() {
        return Err(BridgeError::new(
            "composer-context",
            "This command is only available in a composer window.",
        ));
    }
    window
        .close()
        .map_err(|_| BridgeError::new("window", "Could not close the composer window."))
}

#[tauri::command]
fn native_head_probe() -> String {
    windows::head_capability()
}

#[cfg(feature = "native-head-probe")]
#[derive(serde::Serialize)]
struct NativeHeadResult {
    capability: String,
    conversation_id: String,
    present: bool,
    applied: bool,
}

#[cfg(feature = "native-head-probe")]
fn parse_conversation_id(conversation_id: &str) -> Result<Uuid, String> {
    Uuid::parse_str(conversation_id)
        .map_err(|_| "conversation_id must be an opaque UUID".to_string())
}

/// Explicit opt-in debug probe. No message arrival may call this command.
#[cfg(feature = "native-head-probe")]
#[tauri::command]
async fn show_conversation_head(
    app: tauri::AppHandle,
    conversation_id: String,
    initials: String,
    unread: u32,
) -> Result<NativeHeadResult, String> {
    let conversation = parse_conversation_id(&conversation_id)?;
    #[cfg(target_os = "macos")]
    {
        let initials = initials.chars().take(4).collect::<String>();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        app.run_on_main_thread(move || {
            let _ = sender
                .send(unsafe { windows::show_conversation_head(conversation, &initials, unread) });
        })
        .map_err(|error| error.to_string())?;
        receiver
            .await
            .map_err(|_| "native head UI task was cancelled".to_string())??;
    }
    #[cfg(not(target_os = "macos"))]
    return Err(
        "experimental native head probe is unavailable on this platform; use the main window"
            .into(),
    );
    Ok(NativeHeadResult {
        capability: windows::head_capability(),
        conversation_id: conversation.to_string(),
        present: true,
        applied: true,
    })
}

/// Passive badge update. It never creates, reorders, focuses, or positions a head;
/// callers must retain the main-window fallback when no opted-in head exists.
#[cfg(feature = "native-head-probe")]
#[tauri::command]
async fn update_conversation_head(
    app: tauri::AppHandle,
    conversation_id: String,
    initials: String,
    unread: u32,
) -> Result<NativeHeadResult, String> {
    let conversation = parse_conversation_id(&conversation_id)?;
    #[cfg(target_os = "macos")]
    {
        let initials = initials.chars().take(4).collect::<String>();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        app.run_on_main_thread(move || {
            let _ = sender.send(unsafe {
                windows::update_conversation_head(conversation, &initials, unread)
            });
        })
        .map_err(|error| error.to_string())?;
        let applied = receiver
            .await
            .map_err(|_| "native head UI task was cancelled".to_string())?;
        return Ok(NativeHeadResult {
            capability: windows::head_capability(),
            conversation_id: conversation.to_string(),
            present: applied,
            applied,
        });
    }
    #[cfg(not(target_os = "macos"))]
    Err(
        "experimental native head probe is unavailable on this platform; use the main window"
            .into(),
    )
}

#[cfg(feature = "native-head-probe")]
#[tauri::command]
async fn hide_conversation_head(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<bool, String> {
    let conversation = parse_conversation_id(&conversation_id)?;
    #[cfg(target_os = "macos")]
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        app.run_on_main_thread(move || {
            let _ = sender.send(unsafe { windows::hide_conversation_head(conversation) });
        })
        .map_err(|error| error.to_string())?;
        return receiver
            .await
            .map_err(|_| "native head UI task was cancelled".to_string());
    }
    #[cfg(not(target_os = "macos"))]
    Err(
        "experimental native head probe is unavailable on this platform; use the main window"
            .into(),
    )
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let root = app
                .path()
                .app_data_dir()
                .map_err(|_| "native app data unavailable")?;
            std::fs::create_dir_all(&root).map_err(|_| "could not create native app data")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                    .map_err(|_| "could not protect native app data")?;
            }
            let handle = app.handle().clone();
            // State hints only; no payload, no focus or window changes.
            let notifier: Notifier = Arc::new(move || {
                let _ = handle.emit(STATE_EVENT, ());
            });
            app.manage(AppState::new(root, Arc::new(KeyringStore), notifier));
            let installed = tray::install(app.handle(), |app| {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if new_composer(app.clone(), None).await.is_err() {
                        tray::show_main(&app);
                    }
                });
            });
            TRAY_ACTIVE.store(installed, Ordering::Relaxed);
            // Resume background sync for an already imported credential without any prompt.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let _ = handle.state::<AppState>().session().await;
            });
            #[cfg(all(feature = "native-head-probe", target_os = "macos"))]
            windows::macos::register_app(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == tray::MAIN && TRAY_ACTIVE.load(Ordering::Relaxed) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        });
    #[cfg(feature = "native-head-probe")]
    let builder = builder.invoke_handler(tauri::generate_handler![
        load_state,
        configure_server,
        import_credentials,
        unlock_sync,
        save_draft,
        send_draft,
        mark_seen,
        pick_attachments,
        publish_attachment,
        open_composer,
        show_head,
        update_head,
        hide_head,
        close_composer,
        native_head_probe,
        show_conversation_head,
        update_conversation_head,
        hide_conversation_head
    ]);
    #[cfg(not(feature = "native-head-probe"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        load_state,
        configure_server,
        import_credentials,
        unlock_sync,
        save_draft,
        send_draft,
        mark_seen,
        pick_attachments,
        publish_attachment,
        open_composer,
        show_head,
        update_head,
        hide_head,
        close_composer,
        native_head_probe
    ]);
    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building OpenPush desktop");
    app.run(|app, event| match event {
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => tray::show_main(app),
        RunEvent::Exit => {
            if let Some(state) = app.try_state::<AppState>() {
                if let Ok(mut guard) = state.session.try_lock() {
                    if let Some(session) = guard.take() {
                        session.cancel.cancel();
                    }
                }
            }
        }
        _ => {}
    });
}
