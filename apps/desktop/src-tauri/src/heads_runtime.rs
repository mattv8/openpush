//! Native-owned runtime for floating conversation heads.
use crate::{
    credentials::Binding,
    error::{core_error, BridgeError, BridgeResult},
    heads::{HeadLayout, HeadLayoutStore, Pin, PinStore, MAX_HEADS},
    tray,
    windows::{self, HeadCallback, HeadEvent, HeadFrame, HeadPosition, HeadSpec},
    AppState, STATE_EVENT,
};
use openpush_client_core::ConversationId;
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{AppHandle, Emitter, Listener, Manager, WindowEvent};
use tokio::sync::{mpsc, oneshot};

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PopoutResult {
    pub head_created: bool,
    pub warning: Option<String>,
}

#[derive(Clone)]
struct ActiveHead {
    position: HeadPosition,
    frame: Option<HeadFrame>,
    generation: u64,
    panel: bool,
    name: String,
}

struct RuntimeState {
    binding: Option<Binding>,
    heads: HashMap<String, ActiveHead>,
    generation: u64,
    refreshing: bool,
    queued: bool,
    stopped: bool,
    menu_entries: Option<Vec<(String, String)>>,
    /// Layout cache keyed by conversation_id for the active binding.
    /// Loaded on popout/refresh/expand and updated on every write.
    /// Cleared on binding change/reset.
    layout_cache: HashMap<String, Option<HeadLayout>>,
}

impl RuntimeState {
    fn new(generation: u64) -> Self {
        Self {
            binding: None,
            heads: HashMap::new(),
            generation,
            refreshing: false,
            queued: false,
            stopped: false,
            menu_entries: None,
            layout_cache: HashMap::new(),
        }
    }
}

enum ActivationUi {
    Collapse(BridgeResult<()>),
    Expand(BridgeResult<()>),
}

fn expansion_failed(result: &BridgeResult<Option<ActivationUi>>) -> bool {
    matches!(result, Ok(Some(ActivationUi::Expand(Err(_)))))
}

fn adopt_active_head(
    existing: Option<&ActiveHead>,
    position: HeadPosition,
    generation: u64,
    name: String,
) -> ActiveHead {
    ActiveHead {
        position,
        frame: existing.and_then(|active| active.frame.clone()),
        generation,
        panel: existing.is_some_and(|active| active.panel),
        name,
    }
}

fn pin_limit_reached(persisted: usize, active: usize, already_persisted: bool) -> bool {
    !already_persisted && persisted.max(active) >= MAX_HEADS
}

enum RuntimeAction {
    Activate {
        conversation_id: String,
        generation: u64,
    },
    Dismiss {
        conversation_id: String,
        generation: u64,
    },
    PersistMove {
        conversation_id: String,
        generation: u64,
        position: HeadPosition,
    },
}

pub struct HeadRuntime {
    pins: PinStore,
    layouts: HeadLayoutStore,
    state: Mutex<RuntimeState>,
    actions: mpsc::UnboundedSender<RuntimeAction>,
    panel_writes: Mutex<HashMap<String, PhysicalRect>>,
    panel_debounce: Mutex<HashMap<String, u64>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PhysicalRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone)]
struct RuntimeToken {
    binding: Binding,
    generation: u64,
    session: Arc<crate::session::Session>,
}

#[derive(Clone)]
struct HeadData {
    id: String,
    name: String,
    initials: String,
    unread: u64,
}

/// Installs one serialized refresh listener and one ordered native-event lane.
pub fn install(app: &AppHandle) -> BridgeResult<()> {
    if app.try_state::<HeadRuntime>().is_some() {
        return Ok(());
    }
    let root = app.state::<AppState>().root.clone();
    let generation = app
        .state::<AppState>()
        .head_generation
        .lock()
        .map_err(|_| BridgeError::host_state())?
        .current();
    let (actions, mut receiver) = mpsc::unbounded_channel();
    app.manage(HeadRuntime {
        pins: PinStore::new(&root),
        layouts: HeadLayoutStore::new(&root),
        state: Mutex::new(RuntimeState::new(generation)),
        actions,
        panel_writes: Mutex::new(HashMap::new()),
        panel_debounce: Mutex::new(HashMap::new()),
    });
    let action_app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(action) = receiver.recv().await {
            handle_action(&action_app, action).await;
        }
    });
    let handle = app.clone();
    app.listen(STATE_EVENT, move |_| schedule_refresh(&handle));
    schedule_refresh(app);
    Ok(())
}

fn same_geometry(left: PhysicalRect, right: PhysicalRect) -> bool {
    (left.x - right.x).abs() <= 2
        && (left.y - right.y).abs() <= 2
        && (left.width as i64 - right.width as i64).abs() <= 2
        && (left.height as i64 - right.height as i64).abs() <= 2
}

fn skip_panel_persist(recorded: Option<PhysicalRect>, actual: PhysicalRect) -> bool {
    recorded.is_some_and(|written| same_geometry(written, actual))
}

fn panel_layout_from_physical(frame: &HeadFrame, rect: PhysicalRect) -> crate::heads::PanelLayout {
    let scale = if frame.scale_factor.is_finite() {
        frame.scale_factor.max(1.0)
    } else {
        1.0
    };
    crate::heads::PanelLayout {
        monitor: None,
        width: rect.width as f64 / scale,
        height: rect.height as f64 / scale,
        offset_x: (rect.x as f64 - frame.x) / scale,
        offset_y: (rect.y as f64 - frame.y) / scale,
    }
}

pub(crate) fn record_panel_geometry(app: &AppHandle, label: &str, rect: PhysicalRect) {
    if let Ok(mut writes) = runtime(app).panel_writes.lock() {
        writes.insert(label.to_owned(), rect);
    }
}

pub(crate) fn panel_window_event(window: &tauri::Window, event: &WindowEvent) {
    if tray::composer_conversation(window.label()).is_none()
        || window.app_handle().try_state::<HeadRuntime>().is_none()
    {
        return;
    }
    if matches!(event, WindowEvent::Destroyed) {
        let runtime = runtime(window.app_handle());
        if let Ok(mut writes) = runtime.panel_writes.lock() {
            writes.remove(window.label());
        }
        if let Ok(mut pending) = runtime.panel_debounce.lock() {
            pending.remove(window.label());
        }
        return;
    }
    if !matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_))
        || !is_panel(window.app_handle(), window.label())
    {
        return;
    }
    let Ok(position) = window.outer_position() else {
        return;
    };
    let Ok(size) = window.outer_size() else {
        return;
    };
    let rect = PhysicalRect {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    };
    let app = window.app_handle().clone();
    let label = window.label().to_owned();
    let head_runtime = runtime(&app);
    if head_runtime
        .panel_writes
        .lock()
        .ok()
        .and_then(|writes| writes.get(&label).copied())
        .is_some_and(|written| skip_panel_persist(Some(written), rect))
    {
        return;
    }
    let serial = {
        let Ok(mut pending) = head_runtime.panel_debounce.lock() else {
            return;
        };
        let serial = pending.get(&label).copied().unwrap_or(0).wrapping_add(1);
        pending.insert(label.clone(), serial);
        serial
    };
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let head_runtime = runtime(&app);
        if head_runtime
            .panel_debounce
            .lock()
            .ok()
            .and_then(|pending| pending.get(&label).copied())
            != Some(serial)
        {
            return;
        }
        let Some(window) = app.get_webview_window(&label) else {
            return;
        };
        if !window.is_visible().unwrap_or(false) || window.is_minimized().unwrap_or(false) {
            return;
        }
        let Ok(position) = window.outer_position() else {
            return;
        };
        let Ok(size) = window.outer_size() else {
            return;
        };
        let rect = PhysicalRect {
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
        };
        if skip_panel_persist(
            head_runtime
                .panel_writes
                .lock()
                .ok()
                .and_then(|writes| writes.get(&label).copied()),
            rect,
        ) {
            return;
        }
        let Some(conversation) = tray::composer_conversation(&label) else {
            return;
        };
        let (binding, generation, frame, head_position) = {
            let Ok(state) = head_runtime.state.lock() else {
                return;
            };
            let Some(head) = state.heads.get(&conversation.to_string()) else {
                return;
            };
            (
                state.binding.clone(),
                head.generation,
                head.frame.clone(),
                head.position.clone(),
            )
        };
        let (Some(binding), Some(frame)) = (binding, frame) else {
            return;
        };
        if !app
            .state::<AppState>()
            .head_generation
            .lock()
            .ok()
            .is_some_and(|fence| fence.accepts(generation))
        {
            return;
        }
        let conv_str = conversation.to_string();
        if let Ok(layout) = head_runtime.layouts.save_panel(
            &binding,
            &conv_str,
            head_position,
            panel_layout_from_physical(&frame, rect),
        ) {
            update_layout_cache(&app, &binding, generation, &conv_str, Some(layout));
        }
    });
}

fn runtime(app: &AppHandle) -> tauri::State<'_, HeadRuntime> {
    app.state::<HeadRuntime>()
}

fn schedule_refresh(app: &AppHandle) {
    let start = {
        let runtime = runtime(app);
        let Ok(mut state) = runtime.state.lock() else {
            return;
        };
        if state.stopped {
            return;
        }
        if state.refreshing {
            state.queued = true;
            false
        } else {
            state.refreshing = true;
            true
        }
    };
    if !start {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            refresh(&app).await;
            let again = {
                let runtime = runtime(&app);
                let Ok(mut state) = runtime.state.lock() else {
                    return;
                };
                if state.stopped {
                    state.refreshing = false;
                    state.queued = false;
                    false
                } else if state.queued {
                    state.queued = false;
                    true
                } else {
                    state.refreshing = false;
                    false
                }
            };
            if !again {
                break;
            }
        }
    });
}

async fn refresh(app: &AppHandle) {
    let app_state = app.state::<AppState>();
    let binding = match app_state
        .config()
        .ok()
        .and_then(|config| config.active_binding().cloned())
    {
        Some(binding) => binding,
        None => {
            clear_visible(app).await;
            return;
        }
    };
    let session = match app_state.session().await {
        Ok(Some(session)) if session.binding == binding => session,
        _ => {
            clear_visible(app).await;
            return;
        }
    };
    let (token, stale) = match begin_binding(app, binding.clone(), session.clone()) {
        Ok(value) => value,
        Err(_) => return,
    };
    if !stale.is_empty() {
        let stale_for_ui = stale.clone();
        let _ = on_main_if_current(app, token.clone(), move |app| {
            for id in stale_for_ui {
                let _ = windows::hide_head(app, &id);
            }
        })
        .await;
    }

    let pins = match runtime(app).pins.pins(&binding) {
        Ok(pins) => pins,
        Err(_) => return,
    };
    let pin_ids: HashSet<String> = pins.iter().map(|pin| pin.conversation_id.clone()).collect();
    let cached_names = runtime(app)
        .state
        .lock()
        .ok()
        .map(|state| {
            state
                .heads
                .iter()
                .map(|(id, head)| (id.clone(), head.name.clone()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let queried = crate::sync::blocking({
        let session = session.clone();
        move || resolve_heads(&session, &pin_ids, &cached_names)
    })
    .await;
    let data = match queried {
        Ok(Some(data)) => data,
        Ok(None) => {
            clear_visible(app).await;
            return;
        }
        Err(_) => return, // transient query failures leave the last good registry intact
    };
    if !revalidate_after_await(app, &token).await {
        return;
    }

    let data: HashMap<String, HeadData> = data
        .into_iter()
        .map(|head| (head.id.clone(), head))
        .collect();
    let wanted: HashMap<String, (Pin, HeadData)> = pins
        .into_iter()
        .filter_map(|pin| {
            data.get(&pin.conversation_id)
                .cloned()
                .map(|head| (pin.conversation_id.clone(), (pin, head)))
        })
        .collect();
    let (to_hide, specs) = {
        let runtime = runtime(app);
        let Ok(mut state) = runtime.state.lock() else {
            return;
        };
        if !token_matches_state(app, &state, &token) {
            return;
        }
        let to_hide = state
            .heads
            .keys()
            .filter(|id| !wanted.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        for id in &to_hide {
            state.heads.remove(id);
        }
        let mut specs = Vec::with_capacity(wanted.len());
        for (id, (pin, head)) in wanted {
            let existing = state.heads.get(&id).cloned();
            let position = existing
                .as_ref()
                .map(|active| active.position.clone())
                .unwrap_or(pin.position);
            state.heads.insert(
                id.clone(),
                adopt_active_head(
                    existing.as_ref(),
                    position.clone(),
                    token.generation,
                    head.name.clone(),
                ),
            );
            specs.push((
                HeadSpec {
                    conversation_id: id,
                    generation: token.generation,
                    initials: head.initials,
                    unread: head.unread,
                    position,
                },
                existing.is_some(),
            ));
        }
        (to_hide, specs)
    };

    for id in to_hide {
        let hide_id = id.clone();
        let _ = on_main_if_current(app, token.clone(), move |app| {
            windows::hide_head(app, &hide_id)
        })
        .await;
    }
    for (spec, existed) in specs {
        let spec_id = spec.conversation_id.clone();
        let result = on_main_if_current(app, token.clone(), move |app| {
            if existed {
                windows::update_head(app, &spec)
            } else {
                windows::show_head(app, &spec, callback(app))
            }
        })
        .await;
        if !existed && matches!(result, Ok(Some(Err(_)))) {
            let _ = runtime(app).state.lock().map(|mut state| {
                if state
                    .heads
                    .get(&spec_id)
                    .is_some_and(|head| head.generation == token.generation)
                {
                    state.heads.remove(&spec_id);
                }
            });
        }
    }
    update_tray_menu(app, Some(&token)).await;
}

fn resolve_heads(
    session: &Arc<crate::session::Session>,
    wanted: &HashSet<String>,
    cached_names: &HashMap<String, String>,
) -> BridgeResult<Option<Vec<HeadData>>> {
    if !session_displayable(session)? {
        return Ok(None);
    }
    let conversations = session.client.list_conversations().map_err(core_error)?;
    let drafts = session.client.compose_drafts().map_err(core_error)?;
    let mut result = Vec::with_capacity(wanted.len());
    for conversation in conversations {
        let id = conversation.conversation_id.to_string();
        if !wanted.contains(&id) {
            continue;
        }
        let name = if let Some(name) = cached_names.get(&id) {
            name.clone()
        } else {
            session
                .client
                .messages(conversation.conversation_id)
                .map_err(core_error)?
                .last()
                .map(crate::dto::counterpart)
                .unwrap_or_else(|| "Conversation".into())
        };
        result.push(HeadData {
            id,
            initials: initials(&name),
            name,
            unread: conversation.unread_count,
        });
    }
    for draft in drafts {
        let id = draft.conversation_id.to_string();
        if !wanted.contains(&id) || result.iter().any(|head| head.id == id) {
            continue;
        }
        let name = if draft.recipients.is_empty() {
            "New message".into()
        } else {
            draft.recipients.join(", ")
        };
        result.push(HeadData {
            id,
            initials: initials(&name),
            name,
            unread: 0,
        });
    }
    Ok(Some(result))
}

fn resolve_one(
    session: &Arc<crate::session::Session>,
    conversation: ConversationId,
) -> BridgeResult<HeadData> {
    if !session_displayable(session)? {
        return Err(BridgeError::new(
            "session-unavailable",
            "The local account is no longer available for floating conversations.",
        ));
    }
    if let Some(found) = session
        .client
        .list_conversations()
        .map_err(core_error)?
        .into_iter()
        .find(|item| item.conversation_id == conversation)
    {
        let name = session
            .client
            .messages(conversation)
            .map_err(core_error)?
            .last()
            .map(crate::dto::counterpart)
            .unwrap_or_else(|| "Conversation".into());
        return Ok(HeadData {
            id: conversation.to_string(),
            initials: initials(&name),
            name,
            unread: found.unread_count,
        });
    }
    if let Some(draft) = session
        .client
        .compose_drafts()
        .map_err(core_error)?
        .into_iter()
        .find(|draft| draft.conversation_id == conversation)
    {
        let name = if draft.recipients.is_empty() {
            "New message".into()
        } else {
            draft.recipients.join(", ")
        };
        return Ok(HeadData {
            id: conversation.to_string(),
            initials: initials(&name),
            name,
            unread: 0,
        });
    }
    Err(BridgeError::new(
        "not-found",
        "The conversation is not available locally.",
    ))
}

fn session_displayable(session: &crate::session::Session) -> BridgeResult<bool> {
    // Heads display the same local data as the main window. Sync key state
    // controls transport decryption, not access to an already-open database.
    if session.cancel.is_cancelled() {
        return Ok(false);
    }
    let status = session
        .status
        .lock()
        .map_err(|_| BridgeError::host_state())?;
    Ok(!status.revoked && !session.cancel.is_cancelled())
}

fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| {
            word.chars()
                .next()
                .filter(|character| character.is_alphabetic())
        })
        .flat_map(char::to_uppercase)
        .take(2)
        .collect()
}

fn begin_binding(
    app: &AppHandle,
    binding: Binding,
    session: Arc<crate::session::Session>,
) -> BridgeResult<(RuntimeToken, Vec<String>)> {
    let runtime = runtime(app);
    let mut state = runtime
        .state
        .lock()
        .map_err(|_| BridgeError::host_state())?;
    if state.stopped {
        return Err(BridgeError::new(
            "heads",
            "Floating conversations are stopping.",
        ));
    }
    let binding_changed = state.binding.as_ref() != Some(&binding);
    let generation = {
        let app_state = app.state::<AppState>();
        let mut fence = app_state
            .head_generation
            .lock()
            .map_err(|_| BridgeError::host_state())?;
        if binding_changed {
            fence.invalidate()
        } else {
            fence.current()
        }
    };
    let stale = if binding_changed || state.generation != generation {
        let stale = state.heads.keys().cloned().collect();
        state.generation = generation;
        state.binding = Some(binding.clone());
        state.heads.clear();
        state.layout_cache.clear(); // Clear cache on binding change (N2)
        stale
    } else {
        Vec::new()
    };
    Ok((
        RuntimeToken {
            binding,
            generation: state.generation,
            session,
        },
        stale,
    ))
}

async fn revalidate_after_await(app: &AppHandle, token: &RuntimeToken) -> bool {
    if !token_valid_sync(app, token) {
        return false;
    }
    let current = match app.state::<AppState>().session().await {
        Ok(Some(session)) => session,
        _ => return false,
    };
    if !Arc::ptr_eq(&current, &token.session) || current.binding != token.binding {
        return false;
    }
    crate::sync::blocking({
        let session = current.clone();
        move || session_displayable(&session)
    })
    .await
    .unwrap_or(false)
}

fn token_matches_state(app: &AppHandle, state: &RuntimeState, token: &RuntimeToken) -> bool {
    !state.stopped
        && state.generation == token.generation
        && state.binding.as_ref() == Some(&token.binding)
        && app
            .state::<AppState>()
            .head_generation
            .lock()
            .ok()
            .is_some_and(|fence| fence.accepts(token.generation))
}

fn token_valid_sync(app: &AppHandle, token: &RuntimeToken) -> bool {
    if token.session.cancel.is_cancelled() {
        return false;
    }
    if token
        .session
        .status
        .lock()
        .map_or(true, |status| status.revoked)
    {
        return false;
    }
    if app
        .state::<AppState>()
        .config()
        .ok()
        .and_then(|config| config.active_binding().cloned())
        .as_ref()
        != Some(&token.binding)
    {
        return false;
    }
    runtime(app)
        .state
        .lock()
        .ok()
        .is_some_and(|state| token_matches_state(app, &state, token))
}

async fn on_main_if_current<T, F>(
    app: &AppHandle,
    token: RuntimeToken,
    operation: F,
) -> BridgeResult<Option<T>>
where
    T: Send + 'static,
    F: FnOnce(&AppHandle) -> T + Send + 'static,
{
    let handle = app.clone();
    let (sender, receiver) = oneshot::channel();
    app.run_on_main_thread(move || {
        let result = token_valid_sync(&handle, &token).then(|| operation(&handle));
        let _ = sender.send(result);
    })
    .map_err(|_| BridgeError::new("heads", "Could not schedule floating conversation UI."))?;
    receiver
        .await
        .map_err(|_| BridgeError::new("heads", "Floating conversation UI did not respond."))
}

async fn on_main<T, F>(app: &AppHandle, operation: F) -> BridgeResult<T>
where
    T: Send + 'static,
    F: FnOnce(&AppHandle) -> T + Send + 'static,
{
    let handle = app.clone();
    let (sender, receiver) = oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = sender.send(operation(&handle));
    })
    .map_err(|_| BridgeError::new("heads", "Could not schedule floating conversation UI."))?;
    receiver
        .await
        .map_err(|_| BridgeError::new("heads", "Floating conversation UI did not respond."))
}

fn callback(app: &AppHandle) -> HeadCallback {
    let app = app.clone();
    Arc::new(move |event| match event {
        HeadEvent::Moved {
            conversation_id,
            generation,
            position,
            frame,
            settled,
        } => handle_moved_inline(&app, conversation_id, generation, position, frame, settled),
        HeadEvent::Activate {
            conversation_id,
            generation,
        } => {
            let _ = runtime(&app).actions.send(RuntimeAction::Activate {
                conversation_id,
                generation,
            });
        }
        HeadEvent::Dismiss {
            conversation_id,
            generation,
        } => {
            let _ = runtime(&app).actions.send(RuntimeAction::Dismiss {
                conversation_id,
                generation,
            });
        }
    })
}

fn handle_moved_inline(
    app: &AppHandle,
    conversation_id: String,
    generation: u64,
    position: HeadPosition,
    frame: HeadFrame,
    settled: bool,
) {
    let valid = {
        let runtime = runtime(app);
        let Ok(mut state) = runtime.state.lock() else {
            return;
        };
        if state.stopped
            || state.generation != generation
            || !app
                .state::<AppState>()
                .head_generation
                .lock()
                .ok()
                .is_some_and(|fence| fence.accepts(generation))
        {
            return;
        }
        let Some(head) = state.heads.get_mut(&conversation_id) else {
            return;
        };
        if head.generation != generation {
            return;
        }
        head.position = position.clone();
        head.frame = Some(frame.clone());
        true
    };
    if valid {
        tray::place_head_panel(app, &conversation_id, &frame);
        if settled {
            let _ = runtime(app).actions.send(RuntimeAction::PersistMove {
                conversation_id,
                generation,
                position,
            });
        }
    }
}

async fn handle_action(app: &AppHandle, action: RuntimeAction) {
    match action {
        RuntimeAction::Activate {
            conversation_id,
            generation,
        } => activate(app, &conversation_id, generation).await,
        RuntimeAction::Dismiss {
            conversation_id,
            generation,
        } => {
            if let Err(error) = dismiss_generation(app, &conversation_id, generation).await {
                let message = error.message;
                let _ = on_main(app, move |app| {
                    crate::dialogs::inform(app, "Could not dismiss conversation", &message)
                })
                .await;
            }
        }
        RuntimeAction::PersistMove {
            conversation_id,
            generation,
            position,
        } => {
            let binding = {
                let runtime = runtime(app);
                let Ok(state) = runtime.state.lock() else {
                    return;
                };
                if state.stopped
                    || state.generation != generation
                    || !state.heads.get(&conversation_id).is_some_and(|head| {
                        head.generation == generation
                            && head.position.x == position.x
                            && head.position.y == position.y
                    })
                {
                    return;
                }
                state.binding.clone()
            };
            if let Some(binding) = binding {
                let _ =
                    runtime(app)
                        .pins
                        .update_position(&binding, &conversation_id, position.clone());
                if let Ok(new_layout) =
                    runtime(app)
                        .layouts
                        .save_head(&binding, &conversation_id, position)
                {
                    update_layout_cache(
                        app,
                        &binding,
                        generation,
                        &conversation_id,
                        Some(new_layout),
                    );
                }
            }
        }
    }
}

async fn activate(app: &AppHandle, conversation_id: &str, generation: u64) {
    let _window_permit = match crate::lifecycle::permit_window_creation(app) {
        Ok(permit) => permit,
        Err(_) => return,
    };
    let (binding, frame) = {
        let runtime = runtime(app);
        let Ok(state) = runtime.state.lock() else {
            return;
        };
        if state.stopped || state.generation != generation {
            return;
        }
        let Some(head) = state.heads.get(conversation_id) else {
            return;
        };
        if head.generation != generation {
            return;
        }
        let Some(binding) = state.binding.clone() else {
            return;
        };
        (binding, head.frame.clone())
    };
    let session = match app.state::<AppState>().session().await {
        Ok(Some(session)) if session.binding == binding => session,
        _ => return,
    };
    let token = RuntimeToken {
        binding,
        generation,
        session,
    };
    if !revalidate_after_await(app, &token).await {
        return;
    }
    if load_layout_cache(app, &token, conversation_id)
        .await
        .is_err()
    {
        return;
    }
    {
        let rt = runtime(app);
        let Ok(mut state) = rt.state.lock() else {
            return;
        };
        if !token_matches_state(app, &state, &token) {
            return;
        }
        let Some(head) = state.heads.get_mut(conversation_id) else {
            return;
        };
        if head.generation != generation {
            return;
        }
        head.panel = true;
    }
    let Ok(conversation) = ConversationId::from_str(conversation_id) else {
        return;
    };
    let id = conversation_id.to_owned();
    let result = on_main_if_current(app, token, move |app| {
        let label = format!("{}{}", tray::COMPOSER_PREFIX, id);
        if app
            .get_webview_window(&label)
            .and_then(|window| window.is_visible().ok())
            .unwrap_or(false)
        {
            ActivationUi::Collapse(crate::lifecycle::request_window(
                app,
                &label,
                crate::lifecycle::Action::Collapse,
            ))
        } else {
            let opened = tray::open_head_panel(app, conversation).map(|()| {
                if let Some(frame) = frame.as_ref() {
                    tray::place_head_panel(app, &id, frame);
                }
            });
            ActivationUi::Expand(opened)
        }
    })
    .await;
    if expansion_failed(&result) {
        set_panel(app, conversation_id, false);
    }
    match result {
        Ok(Some(ActivationUi::Expand(Err(_)))) => {}
        Ok(Some(ActivationUi::Expand(Ok(())) | ActivationUi::Collapse(Ok(())))) => {
            let _ = app.emit(STATE_EVENT, ());
        }
        // Busy/failed collapse keeps panel mode so a later click still uses the
        // draft-safe collapse path. Stale tokens are handled by the fence.
        Ok(Some(ActivationUi::Collapse(Err(_)))) | Ok(None) | Err(_) => {}
    }
}

pub async fn popout(app: &AppHandle, conversation: ConversationId) -> BridgeResult<PopoutResult> {
    let app_state = app.state::<AppState>();
    let binding = app_state
        .config()?
        .active_binding()
        .cloned()
        .ok_or_else(BridgeError::no_session)?;
    let session = app_state.require_session().await?;
    if session.binding != binding {
        return Err(BridgeError::no_session());
    }
    let (token, stale) = begin_binding(app, binding.clone(), session.clone())?;
    if !stale.is_empty() {
        let _ = on_main_if_current(app, token.clone(), move |app| {
            for id in stale {
                let _ = windows::hide_head(app, &id);
            }
        })
        .await?;
    }
    let head = crate::sync::blocking({
        let session = session.clone();
        move || resolve_one(&session, conversation)
    })
    .await?;
    if !revalidate_after_await(app, &token).await {
        return Err(BridgeError::new(
            "session-changed",
            "The active account changed while opening the floating conversation.",
        ));
    }

    let saved = runtime(app)
        .pins
        .pins(&binding)
        .map_err(|message| BridgeError::new("heads", message))?;
    let persisted = saved
        .iter()
        .find(|pin| pin.conversation_id == head.id)
        .cloned();
    let saved_layout = load_layout_cache(app, &token, &head.id).await?;
    let prior = runtime(app)
        .state
        .lock()
        .map_err(|_| BridgeError::host_state())?
        .heads
        .get(&head.id)
        .cloned();
    if pin_limit_reached(
        saved.len(),
        runtime(app)
            .state
            .lock()
            .map_err(|_| BridgeError::host_state())?
            .heads
            .len(),
        persisted.is_some(),
    ) {
        return Err(BridgeError::new(
            "limit",
            "You can pin at most eight floating conversations.",
        ));
    }
    let position = prior
        .as_ref()
        .map(|active| active.position.clone())
        .or_else(|| persisted.as_ref().map(|pin| pin.position.clone()))
        .or_else(|| saved_layout.and_then(|layout| layout.head))
        .unwrap_or_else(|| next_position(saved.len()));
    let spec = HeadSpec {
        conversation_id: head.id.clone(),
        generation: token.generation,
        initials: head.initials,
        unread: head.unread,
        position: position.clone(),
    };
    {
        let rt = runtime(app);
        let mut state = rt.state.lock().map_err(|_| BridgeError::host_state())?;
        if !token_matches_state(app, &state, &token) {
            return Err(BridgeError::new(
                "session-changed",
                "The active account changed.",
            ));
        }
        state.heads.insert(
            head.id.clone(),
            ActiveHead {
                position: position.clone(),
                frame: prior.as_ref().and_then(|active| active.frame.clone()),
                generation: token.generation,
                panel: true,
                name: head.name,
            },
        );
    }

    let spec_for_ui = spec.clone();
    let native = on_main_if_current(app, token.clone(), move |app| {
        windows::show_head(app, &spec_for_ui, callback(app))
    })
    .await?;
    let native_error = match native {
        Some(Ok(())) => None,
        Some(Err(message)) => Some(message),
        None => {
            restore_prior(app, &spec.conversation_id, prior.clone());
            return Err(BridgeError::new(
                "session-changed",
                "The active account changed.",
            ));
        }
    };
    if let Some(message) = native_error {
        restore_prior(app, &spec.conversation_id, prior);
        let _ = on_main_if_current(app, token, move |app| {
            tray::open_composer(app, conversation)
        })
        .await?;
        return Ok(PopoutResult {
            head_created: false,
            warning: Some(format!(
                "Could not show a floating conversation; opened a composer instead. {message}"
            )),
        });
    }

    let created = persisted.is_none();
    if created {
        if let Err(message) = runtime(app).pins.upsert(
            &binding,
            Pin {
                conversation_id: spec.conversation_id.clone(),
                position: spec.position.clone(),
            },
        ) {
            if prior.is_none() {
                let id = spec.conversation_id.clone();
                let _ =
                    on_main_if_current(app, token.clone(), move |app| windows::hide_head(app, &id))
                        .await;
            }
            restore_prior(app, &spec.conversation_id, prior);
            return Err(BridgeError::new("heads", message));
        }
    }

    let id = spec.conversation_id.clone();
    let frame = runtime(app)
        .state
        .lock()
        .ok()
        .and_then(|state| state.heads.get(&id).and_then(|head| head.frame.clone()));
    let panel = on_main_if_current(app, token.clone(), move |app| {
        tray::open_head_panel(app, conversation)?;
        if let Some(frame) = frame.as_ref() {
            tray::place_head_panel(app, &id, frame);
        }
        Ok::<_, BridgeError>(())
    })
    .await?;
    if !matches!(panel, Some(Ok(()))) {
        set_panel(app, &spec.conversation_id, false);
        let _ = app.emit(STATE_EVENT, ());
        return match panel {
            Some(Err(error)) => Err(error),
            _ => Err(BridgeError::new(
                "session-changed",
                "The active account changed.",
            )),
        };
    }
    update_tray_menu(app, Some(&token)).await;
    let _ = app.emit(STATE_EVENT, ());
    Ok(PopoutResult {
        head_created: created,
        warning: None,
    })
}

fn next_position(count: usize) -> HeadPosition {
    let slot = count.min(MAX_HEADS - 1) as f64;
    HeadPosition {
        monitor: None,
        x: 24.0 + slot * 68.0,
        y: 80.0 + (slot % 3.0) * 12.0,
    }
}

fn restore_prior(app: &AppHandle, conversation_id: &str, prior: Option<ActiveHead>) {
    if let Ok(mut state) = runtime(app).state.lock() {
        if let Some(prior) = prior {
            state.heads.insert(conversation_id.to_owned(), prior);
        } else {
            state.heads.remove(conversation_id);
        }
    }
}

pub async fn dismiss(app: &AppHandle, conversation_id: &str) -> BridgeResult<()> {
    let generation = runtime(app)
        .state
        .lock()
        .map_err(|_| BridgeError::host_state())?
        .heads
        .get(conversation_id)
        .map(|head| head.generation)
        .ok_or_else(|| BridgeError::new("not-found", "The floating conversation is not open."))?;
    dismiss_generation(app, conversation_id, generation).await
}

pub(crate) async fn dismiss_generation(
    app: &AppHandle,
    conversation_id: &str,
    generation: u64,
) -> BridgeResult<()> {
    let binding = {
        let rt = runtime(app);
        let state = rt.state.lock().map_err(|_| BridgeError::host_state())?;
        state
            .heads
            .get(conversation_id)
            .filter(|head| head.generation == generation)
            .ok_or_else(|| {
                BridgeError::new("not-found", "The floating conversation is not open.")
            })?;
        state.binding.clone().ok_or_else(BridgeError::no_session)?
    };
    let session = app
        .state::<AppState>()
        .session()
        .await?
        .filter(|session| session.binding == binding)
        .ok_or_else(BridgeError::no_session)?;
    let token = RuntimeToken {
        binding: binding.clone(),
        generation,
        session,
    };
    if !revalidate_after_await(app, &token).await {
        return Err(BridgeError::new(
            "session-changed",
            "The active account changed.",
        ));
    }
    let removed = runtime(app)
        .pins
        .remove(&binding, conversation_id)
        .map_err(|message| BridgeError::new("heads", message))?;
    let id = conversation_id.to_owned();
    let hidden =
        on_main_if_current(app, token.clone(), move |app| windows::hide_head(app, &id)).await?;
    if !matches!(hidden, Some(Ok(()))) {
        if let Some(pin) = removed {
            let _ = runtime(app).pins.upsert(&binding, pin);
        }
        return match hidden {
            Some(Err(message)) => Err(BridgeError::new("heads", message)),
            _ => Err(BridgeError::new(
                "session-changed",
                "The active account changed.",
            )),
        };
    }
    if let Ok(mut state) = runtime(app).state.lock() {
        if state
            .heads
            .get(conversation_id)
            .is_some_and(|head| head.generation == generation)
        {
            state.heads.remove(conversation_id);
        }
    }
    let id = conversation_id.to_owned();
    let cleanup = on_main_if_current(app, token.clone(), move |app| {
        let label = format!("{}{}", tray::COMPOSER_PREFIX, id);
        let hidden_composer = app
            .get_webview_window(&label)
            .is_some_and(|window| !window.is_visible().unwrap_or(false));
        tray::set_head_panel(app, &id, false);
        if hidden_composer {
            if let Err(error) =
                crate::lifecycle::request_window(app, &label, crate::lifecycle::Action::Close)
            {
                if let Some(window) = app.get_webview_window(&label) {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
                return Err(error);
            }
        }
        Ok(())
    })
    .await?;
    update_tray_menu(app, Some(&token)).await;
    let _ = app.emit(STATE_EVENT, ());
    match cleanup {
        Some(result) => result,
        None => Err(BridgeError::new(
            "session-changed",
            "The active account changed while cleaning up the composer.",
        )),
    }
}

pub fn snapshot(app: &AppHandle, window_label: &str) -> (Vec<String>, bool) {
    let runtime = runtime(app);
    let Ok(state) = runtime.state.lock() else {
        return (Vec::new(), false);
    };
    let panel = tray::composer_conversation(window_label).is_some_and(|id| {
        state
            .heads
            .get(&id.to_string())
            .is_some_and(|head| head.panel)
    });
    let ids = state
        .binding
        .as_ref()
        .and_then(|binding| runtime.pins.pins(binding).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|pin| state.heads.contains_key(&pin.conversation_id))
        .map(|pin| pin.conversation_id)
        .collect();
    (ids, panel)
}

/// Load once before expansion, including activation of a pin restored at startup.
/// Native drag callbacks subsequently consult memory only.
async fn load_layout_cache(
    app: &AppHandle,
    token: &RuntimeToken,
    conversation_id: &str,
) -> BridgeResult<Option<HeadLayout>> {
    let handle = app.clone();
    let binding = token.binding.clone();
    let id = conversation_id.to_owned();
    let layout = crate::sync::blocking(move || {
        Ok(runtime(&handle)
            .layouts
            .layout(&binding, &id)
            .ok()
            .flatten())
    })
    .await?;
    if !revalidate_after_await(app, token).await {
        return Err(BridgeError::new(
            "session-changed",
            "The active account changed.",
        ));
    }
    update_layout_cache(
        app,
        &token.binding,
        token.generation,
        conversation_id,
        layout.clone(),
    );
    Ok(layout)
}

/// Update the cache after a write.
fn update_layout_cache(
    app: &AppHandle,
    binding: &Binding,
    generation: u64,
    conversation_id: &str,
    layout: Option<crate::heads::HeadLayout>,
) {
    if let Ok(mut state) = runtime(app).state.lock() {
        cache_layout(&mut state, binding, generation, conversation_id, layout);
    }
}

fn cache_layout(
    state: &mut RuntimeState,
    binding: &Binding,
    generation: u64,
    id: &str,
    layout: Option<HeadLayout>,
) {
    if state.stopped || state.generation != generation || state.binding.as_ref() != Some(binding) {
        return;
    }
    let cached = state.layout_cache.get(id).and_then(Option::as_ref);
    if cached.is_some_and(|cached| {
        layout
            .as_ref()
            .is_none_or(|new| new.updated < cached.updated)
    }) {
        return;
    }
    state.layout_cache.insert(id.to_owned(), layout);
}

pub fn panel_layout(app: &AppHandle, conversation_id: &str) -> Option<crate::heads::PanelLayout> {
    let runtime = runtime(app);
    let state = runtime.state.lock().ok()?;
    // Check cache first (N2: no disk I/O on follow path)
    if let Some(cached) = state.layout_cache.get(conversation_id) {
        return cached.as_ref().and_then(|layout| layout.panel.clone());
    }
    None
}

pub fn is_panel(app: &AppHandle, window_label: &str) -> bool {
    let Some(id) = tray::composer_conversation(window_label) else {
        return false;
    };
    runtime(app)
        .state
        .lock()
        .ok()
        .and_then(|state| state.heads.get(&id.to_string()).map(|head| head.panel))
        .unwrap_or(false)
}

pub(crate) fn head_generation(app: &AppHandle, conversation_id: &str) -> BridgeResult<u64> {
    runtime(app)
        .state
        .lock()
        .map_err(|_| BridgeError::host_state())?
        .heads
        .get(conversation_id)
        .map(|head| head.generation)
        .ok_or_else(|| BridgeError::new("not-found", "The floating conversation is not open."))
}

pub fn conversation_is_panel(app: &AppHandle, conversation_id: &str) -> bool {
    runtime(app)
        .state
        .lock()
        .ok()
        .and_then(|state| state.heads.get(conversation_id).map(|head| head.panel))
        .unwrap_or(false)
}

pub fn activate_from_tray(app: &AppHandle, conversation_id: &str) {
    let event = runtime(app).state.lock().ok().and_then(|state| {
        state
            .heads
            .get(conversation_id)
            .map(|head| RuntimeAction::Activate {
                conversation_id: conversation_id.to_owned(),
                generation: head.generation,
            })
    });
    if let Some(event) = event {
        let _ = runtime(app).actions.send(event);
    }
}

pub fn dismiss_from_tray(app: &AppHandle, conversation_id: &str) {
    let event = runtime(app).state.lock().ok().and_then(|state| {
        state
            .heads
            .get(conversation_id)
            .map(|head| RuntimeAction::Dismiss {
                conversation_id: conversation_id.to_owned(),
                generation: head.generation,
            })
    });
    if let Some(event) = event {
        let _ = runtime(app).actions.send(event);
    }
}

pub async fn reset(app: &AppHandle) -> BridgeResult<()> {
    {
        let runtime = runtime(app);
        let mut state = runtime
            .state
            .lock()
            .map_err(|_| BridgeError::host_state())?;
        let generation = app
            .state::<AppState>()
            .head_generation
            .lock()
            .map_err(|_| BridgeError::host_state())?
            .invalidate();
        state.generation = generation;
        state.heads.clear();
        state.binding = None;
        state.queued = false;
        state.layout_cache.clear(); // Clear cache on binding change (N2)
    }
    on_main(app, windows::clear_heads).await?;
    update_tray_menu(app, None).await;
    Ok(())
}

pub fn clear_on_exit(app: &AppHandle) {
    if let Ok(mut state) = runtime(app).state.lock() {
        if let Ok(mut fence) = app.state::<AppState>().head_generation.lock() {
            state.generation = fence.invalidate();
        }
        state.stopped = true;
        state.refreshing = false;
        state.queued = false;
        state.heads.clear();
        state.binding = None;
        state.layout_cache.clear(); // Clear cache on exit (N2)
    }
    let app = app.clone();
    let _ = app
        .clone()
        .run_on_main_thread(move || windows::clear_heads(&app));
}

async fn clear_visible(app: &AppHandle) {
    let labels = {
        let runtime = runtime(app);
        let Ok(mut state) = runtime.state.lock() else {
            return;
        };
        if state.stopped {
            return;
        }
        if let Ok(mut fence) = app.state::<AppState>().head_generation.lock() {
            state.generation = fence.invalidate();
        }
        let labels = state
            .heads
            .iter()
            .filter(|(_, head)| head.panel)
            .map(|(id, _)| format!("{}{}", tray::COMPOSER_PREFIX, id))
            .collect::<Vec<_>>();
        state.heads.clear();
        state.binding = None;
        state.layout_cache.clear(); // Clear cache on clear visible (N2)
        labels
    };
    let _ = on_main(app, move |app| {
        windows::clear_heads(app);
        for label in labels {
            if let Some(window) = app.get_webview_window(&label) {
                let _ = window.hide();
            }
        }
    })
    .await;
    update_tray_menu(app, None).await;
}

fn set_panel(app: &AppHandle, conversation_id: &str, panel: bool) {
    if let Ok(mut state) = runtime(app).state.lock() {
        if let Some(head) = state.heads.get_mut(conversation_id) {
            head.panel = panel;
        }
    }
}

async fn update_tray_menu(app: &AppHandle, token: Option<&RuntimeToken>) {
    let entries = runtime(app)
        .state
        .lock()
        .ok()
        .map(|state| {
            let mut entries = state
                .heads
                .iter()
                .map(|(id, head)| (id.clone(), head.name.clone()))
                .collect::<Vec<_>>();
            entries.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            entries
        })
        .unwrap_or_default();
    if runtime(app)
        .state
        .lock()
        .ok()
        .is_some_and(|state| state.menu_entries.as_ref() == Some(&entries))
    {
        return;
    }
    let applied = if let Some(token) = token {
        on_main_if_current(app, token.clone(), {
            let entries = entries.clone();
            move |app| tray::set_floating_conversations(app, &entries)
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
    } else {
        on_main(app, {
            let entries = entries.clone();
            move |app| tray::set_floating_conversations(app, &entries)
        })
        .await
        .unwrap_or(false)
    };
    if applied {
        if let Ok(mut state) = runtime(app).state.lock() {
            state.menu_entries = Some(entries);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heads::PanelLayout;
    use crate::tests::{fixture_at, input, open};
    use openpush_client_core::IncomingSms;
    use std::sync::atomic::Ordering;

    fn locally_saved_draft_without_sync_unlock() -> (
        crate::tests::Fixture,
        Arc<crate::session::Session>,
        ConversationId,
    ) {
        let fixture = fixture_at("http://127.0.0.1:9");
        let session = Arc::new(open(&fixture, &fixture.binding, &[]));
        let draft = session
            .save_draft(&input(
                "draft-new",
                "",
                "offline draft",
                &["+15555550100"],
                "0",
            ))
            .unwrap();
        let conversation = ConversationId::from_str(&draft.conversation_id).unwrap();
        (fixture, session, conversation)
    }

    #[test]
    fn initials_use_only_leading_unicode_letters() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("+18016944617"), "");
        assert_eq!(initials("  7 Bob  Élodie"), "BÉ");
        assert_eq!(initials("李 小龍"), "李小");
    }

    #[test]
    fn programmatic_geometry_guard_tolerates_two_physical_pixels() {
        let written = PhysicalRect {
            x: 10,
            y: 20,
            width: 680,
            height: 880,
        };
        assert!(same_geometry(
            written,
            PhysicalRect {
                x: 12,
                y: 18,
                width: 682,
                height: 878
            }
        ));
        assert!(!same_geometry(written, PhysicalRect { x: 13, ..written }));
    }

    #[test]
    fn matching_recorded_geometry_skips_persist() {
        let rect = PhysicalRect {
            x: 10,
            y: 20,
            width: 340,
            height: 440,
        };
        assert!(skip_panel_persist(Some(rect), rect));
        assert!(!skip_panel_persist(None, rect));
    }

    #[test]
    fn panel_geometry_is_persisted_in_logical_points_relative_to_head() {
        let frame = HeadFrame {
            x: 200.0,
            y: 100.0,
            width: 112.0,
            height: 112.0,
            work_x: 0.0,
            work_y: 0.0,
            work_width: 2000.0,
            work_height: 1600.0,
            scale_factor: 2.0,
        };
        let layout = panel_layout_from_physical(
            &frame,
            PhysicalRect {
                x: 240,
                y: 228,
                width: 680,
                height: 880,
            },
        );
        assert_eq!(
            (
                layout.width,
                layout.height,
                layout.offset_x,
                layout.offset_y
            ),
            (340.0, 440.0, 20.0, 64.0)
        );
    }

    #[test]
    fn resolved_heads_clear_unread_after_scoped_mark_seen() {
        let fixture = fixture_at("http://127.0.0.1:9");
        let session = Arc::new(open(&fixture, &fixture.binding, &[]));
        session
            .client
            .unlock(&fixture.profile, &fixture.header, crate::tests::PHRASE)
            .unwrap();
        let draft = session
            .save_draft(&input("head-unread-draft", "", "", &["+15555550101"], "0"))
            .unwrap();
        let conversation = ConversationId::from_str(&draft.conversation_id).unwrap();
        let captured = session
            .client
            .capture_incoming(IncomingSms {
                conversation_id: Some(conversation),
                sender_address: "+15555550101".into(),
                body: "new incoming message".into(),
                provider_message_id: Some("head-unread-clear".into()),
                imported: false,
            })
            .unwrap();
        assert_eq!(captured.conversation_id, conversation);
        let id = conversation.to_string();
        let wanted = HashSet::from([id.clone()]);
        assert!(
            resolve_heads(&session, &wanted, &HashMap::new())
                .unwrap()
                .unwrap()[0]
                .unread
                >= 1
        );
        let message = session.client.messages(conversation).unwrap()[0]
            .payload
            .record
            .message_id
            .to_string();
        session
            .mark_seen_scoped(&[message], Some(conversation))
            .unwrap();
        assert_eq!(
            resolve_heads(&session, &wanted, &HashMap::new())
                .unwrap()
                .unwrap()[0]
                .unread,
            0
        );
    }

    #[test]
    fn locally_saved_draft_resolves_without_an_active_sync_epoch() {
        let (_fixture, session, conversation) = locally_saved_draft_without_sync_unlock();
        assert_eq!(session.client.key_status().unwrap().active_epoch, None);

        let head = resolve_one(&session, conversation).unwrap();

        assert_eq!(head.id, conversation.to_string());
        assert_eq!(head.name, "+15555550100");
        let restored = resolve_heads(
            &session,
            &HashSet::from([conversation.to_string()]),
            &HashMap::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, conversation.to_string());
    }

    #[test]
    fn locally_saved_draft_resolves_with_a_locked_active_sync_epoch() {
        let fixture = fixture_at("http://127.0.0.1:9");
        let conversation = {
            let session = open(&fixture, &fixture.binding, &[]);
            session
                .client
                .unlock(&fixture.profile, &fixture.header, crate::tests::PHRASE)
                .unwrap();
            let draft = session
                .save_draft(&input(
                    "draft-new",
                    "",
                    "offline draft",
                    &["+15555550100"],
                    "0",
                ))
                .unwrap();
            ConversationId::from_str(&draft.conversation_id).unwrap()
        };
        let session = Arc::new(open(&fixture, &fixture.binding, &[]));
        let keys = session.client.key_status().unwrap();
        assert_eq!(keys.active_epoch, Some(1));
        assert!(!keys.unlocked_epochs.contains(&1));
        session.status.lock().unwrap().vault = Some(crate::session::VaultSummary {
            epoch: 2,
            fingerprint: "newer-sync-epoch".into(),
        });

        assert_eq!(
            resolve_one(&session, conversation).unwrap().id,
            conversation.to_string()
        );
        let restored = resolve_heads(
            &session,
            &HashSet::from([conversation.to_string()]),
            &HashMap::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, conversation.to_string());
    }

    #[test]
    fn locally_saved_draft_remains_available_after_sync_passphrase_mismatch() {
        let (_fixture, session, conversation) = locally_saved_draft_without_sync_unlock();
        session.mismatch.store(true, Ordering::Release);
        assert_eq!(
            resolve_one(&session, conversation).unwrap().id,
            conversation.to_string()
        );
        let restored = resolve_heads(
            &session,
            &HashSet::from([conversation.to_string()]),
            &HashMap::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, conversation.to_string());
    }

    #[test]
    fn local_head_resolution_rejects_cancelled_session() {
        let (_fixture, session, conversation) = locally_saved_draft_without_sync_unlock();
        session.cancel.cancel();
        assert!(!session_displayable(&session).unwrap());
        assert_eq!(
            resolve_one(&session, conversation).err().unwrap().code,
            "session-unavailable"
        );
    }

    #[test]
    fn local_head_resolution_rejects_revoked_session() {
        let (_fixture, session, conversation) = locally_saved_draft_without_sync_unlock();
        session.status.lock().unwrap().revoked = true;
        assert!(!session_displayable(&session).unwrap());
        assert_eq!(
            resolve_one(&session, conversation).err().unwrap().code,
            "session-unavailable"
        );
    }

    #[test]
    fn local_head_resolution_rejects_unknown_conversation() {
        let (_fixture, session, _) = locally_saved_draft_without_sync_unlock();
        assert_eq!(
            resolve_one(&session, ConversationId::new())
                .err()
                .unwrap()
                .code,
            "not-found"
        );
    }

    fn position() -> HeadPosition {
        HeadPosition {
            monitor: None,
            x: 10.0,
            y: 20.0,
        }
    }

    #[test]
    fn same_binding_adoption_preserves_panel_and_authoritative_frame() {
        let frame = HeadFrame {
            x: 1.0,
            y: 2.0,
            width: 56.0,
            height: 56.0,
            work_x: 0.0,
            work_y: 0.0,
            work_width: 1000.0,
            work_height: 800.0,
            scale_factor: 2.0,
        };
        let existing = ActiveHead {
            position: position(),
            frame: Some(frame.clone()),
            generation: 4,
            panel: true,
            name: "Old".into(),
        };
        let adopted = adopt_active_head(Some(&existing), position(), 4, "New".into());
        assert!(adopted.panel);
        assert_eq!(adopted.frame.unwrap().x, frame.x);
        assert_eq!(adopted.name, "New");
    }

    #[test]
    fn failed_collapse_keeps_panel_but_failed_expansion_clears_it() {
        let error = || BridgeError::new("window", "failed");
        assert!(!expansion_failed(&Ok(Some(ActivationUi::Collapse(Err(
            error()
        ))))));
        assert!(expansion_failed(&Ok(Some(ActivationUi::Expand(Err(
            error()
        ))))));
    }

    #[test]
    fn pin_limit_counts_both_persisted_and_active_heads() {
        assert!(pin_limit_reached(MAX_HEADS, 1, false));
        assert!(pin_limit_reached(1, MAX_HEADS, false));
        assert!(!pin_limit_reached(MAX_HEADS, MAX_HEADS, true));
    }

    #[test]
    fn restored_layout_cache_rejects_stale_generations_bindings_and_writes() {
        let root = tempfile::tempdir().unwrap();
        let binding = Binding {
            origin: "https://example.test".into(),
            vault_id: "vault".into(),
            device_id: "device".into(),
        };
        let id = uuid::Uuid::new_v4().to_string();
        let store = HeadLayoutStore::new(root.path());
        let saved = store
            .save_panel(
                &binding,
                &id,
                position(),
                PanelLayout {
                    monitor: None,
                    width: 620.0,
                    height: 500.0,
                    offset_x: -20.0,
                    offset_y: 64.0,
                },
            )
            .unwrap();
        drop(store);
        let restored = HeadLayoutStore::new(root.path())
            .layout(&binding, &id)
            .unwrap();
        let mut state = RuntimeState::new(1);
        state.binding = Some(binding.clone());
        cache_layout(&mut state, &binding, 1, &id, restored);
        let mut old = saved;
        old.updated -= 1;
        old.panel.as_mut().unwrap().width = 340.0;
        cache_layout(&mut state, &binding, 1, &id, Some(old.clone()));
        cache_layout(&mut state, &binding, 1, &id, None);
        assert_eq!(
            state.layout_cache[&id]
                .as_ref()
                .unwrap()
                .panel
                .as_ref()
                .unwrap()
                .width,
            620.0
        );
        state.layout_cache.clear();
        state.generation = 2;
        cache_layout(&mut state, &binding, 1, &id, Some(old.clone()));
        assert!(state.layout_cache.is_empty());
        let other = Binding {
            device_id: "other".into(),
            ..binding
        };
        cache_layout(&mut state, &other, 2, &id, Some(old));
        assert!(state.layout_cache.is_empty());
    }
}
