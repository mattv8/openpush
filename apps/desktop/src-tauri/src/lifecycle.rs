//! Native-owned draft-safe lifecycle request coordinator.
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
use tauri::{Emitter, EventTarget, Manager};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Quit,
    Close,
    Collapse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Window(Action),
    Quit,
    Switch,
}

impl Operation {
    fn action(self) -> Action {
        match self {
            Self::Window(action) => action,
            Self::Quit => Action::Quit,
            // Switching is an internal reason for flushing. The UI only needs
            // the existing close/save behavior; native owns the later teardown.
            Self::Switch => Action::Close,
        }
    }
}

pub fn close_operation(is_main: bool, tray_available: bool, is_panel: bool) -> Operation {
    if is_main && !tray_available {
        Operation::Quit
    } else if is_panel {
        Operation::Window(Action::Collapse)
    } else {
        Operation::Window(Action::Close)
    }
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub id: String,
    pub action: Action,
    #[serde(skip_serializing)]
    pub labels: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub operation: Operation,
    pub labels: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Rejected,
    TimedOut,
    EmitFailed,
    ApplyFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub operation: Operation,
    pub request_id: String,
    pub labels: BTreeSet<String>,
    pub restore_labels: BTreeSet<String>,
    pub failed_label: Option<String>,
    pub kind: FailureKind,
}

impl Failure {
    fn message(&self) -> &'static str {
        match self.kind {
            FailureKind::Rejected => "Draft saving failed. OpenPush remains available.",
            FailureKind::TimedOut => "Draft saving timed out. OpenPush remains available.",
            FailureKind::EmitFailed => {
                "A window closed before OpenPush could request draft saving."
            }
            FailureKind::ApplyFailed => "The requested window action could not be completed.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    Pending,
    Complete(Completion),
    Failed(Failure),
}

pub struct Started {
    pub request: Request,
    pub done: oneshot::Receiver<Result<Completion, Failure>>,
}

#[derive(Default)]
pub struct Coordinator {
    pending: Option<Pending>,
}

struct Pending {
    request: Request,
    operation: Operation,
    remaining: BTreeSet<String>,
    deadline: Instant,
    done: oneshot::Sender<Result<Completion, Failure>>,
}

impl Coordinator {
    #[cfg(test)]
    pub fn begin_at(
        &mut self,
        operation: Operation,
        labels: impl IntoIterator<Item = String>,
        timeout: Duration,
        now: Instant,
    ) -> Result<Started, &'static str> {
        self.begin_at_with_id(
            operation,
            labels,
            timeout,
            now,
            uuid::Uuid::new_v4().to_string(),
        )
    }

    fn begin_at_with_id(
        &mut self,
        operation: Operation,
        labels: impl IntoIterator<Item = String>,
        timeout: Duration,
        now: Instant,
        id: String,
    ) -> Result<Started, &'static str> {
        if self.pending.is_some() {
            return Err("A lifecycle request is already in progress.");
        }
        let labels: BTreeSet<_> = labels.into_iter().collect();
        let request = Request {
            id,
            action: operation.action(),
            labels: labels.clone(),
        };
        let (sender, done) = oneshot::channel();
        if labels.is_empty() {
            let _ = sender.send(Ok(Completion { operation, labels }));
        } else {
            self.pending = Some(Pending {
                request: request.clone(),
                operation,
                remaining: labels,
                deadline: now + timeout,
                done: sender,
            });
        }
        Ok(Started { request, done })
    }

    pub fn acknowledge_at(
        &mut self,
        id: &str,
        label: &str,
        ok: bool,
        now: Instant,
    ) -> Result<Progress, &'static str> {
        let pending = self
            .pending
            .as_ref()
            .ok_or("No lifecycle request is pending.")?;
        if pending.request.id != id || !pending.request.labels.contains(label) {
            return Err("This lifecycle acknowledgement is not authorized.");
        }
        if !pending.remaining.contains(label) {
            return Err("This lifecycle acknowledgement was already received.");
        }
        if now >= pending.deadline {
            let failure = self
                .take_failure(FailureKind::TimedOut, None)
                .expect("the checked request is pending");
            return Ok(Progress::Failed(failure));
        }
        if !ok {
            let failure = self
                .take_failure(FailureKind::Rejected, Some(label.to_owned()))
                .expect("the checked request is pending");
            return Ok(Progress::Failed(failure));
        }

        let pending = self
            .pending
            .as_mut()
            .expect("the checked request is pending");
        pending.remaining.remove(label);
        if !pending.remaining.is_empty() {
            return Ok(Progress::Pending);
        }
        let pending = self.pending.take().expect("the checked request is pending");
        let completion = Completion {
            operation: pending.operation,
            labels: pending.request.labels,
        };
        let _ = pending.done.send(Ok(completion.clone()));
        Ok(Progress::Complete(completion))
    }

    pub fn expire(&mut self, id: &str, now: Instant) -> Option<Failure> {
        let pending = self.pending.as_ref()?;
        if pending.request.id != id || now < pending.deadline {
            return None;
        }
        self.take_failure(FailureKind::TimedOut, None)
    }

    pub fn cancel(
        &mut self,
        id: &str,
        kind: FailureKind,
        failed_label: Option<String>,
    ) -> Option<Failure> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request.id == id)
        {
            self.take_failure(kind, failed_label)
        } else {
            None
        }
    }

    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn take_failure(&mut self, kind: FailureKind, failed_label: Option<String>) -> Option<Failure> {
        let pending = self.pending.take()?;
        let failure = Failure {
            operation: pending.operation,
            request_id: pending.request.id,
            labels: pending.request.labels,
            restore_labels: pending.remaining,
            failed_label,
            kind,
        };
        let _ = pending.done.send(Err(failure.clone()));
        Some(failure)
    }
}

#[derive(Default)]
pub struct TopologyGate {
    active: Option<(String, Operation)>,
    creators: usize,
}

impl TopologyGate {
    fn reserve(&mut self, id: String, operation: Operation) -> Result<(), &'static str> {
        if self.active.is_some() || self.creators > 0 {
            return Err("A lifecycle topology change or window creation is already in progress.");
        }
        self.active = Some((id, operation));
        Ok(())
    }

    fn release(&mut self, id: &str) -> bool {
        if self.active.as_ref().is_some_and(|(active, _)| active == id) {
            self.active = None;
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    fn blocked(&self) -> bool {
        self.active.is_some()
    }
}

pub const REQUEST_EVENT: &str = "openpush://lifecycle-request";
pub const FINISHED_EVENT: &str = "openpush://lifecycle-finished";

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Finished {
    id: String,
    ok: bool,
}

pub struct WindowPermit {
    app: tauri::AppHandle,
}

impl Drop for WindowPermit {
    fn drop(&mut self) {
        if let Ok(mut gate) = self.app.state::<crate::AppState>().lifecycle_gate.lock() {
            gate.creators = gate.creators.saturating_sub(1);
        }
    }
}

pub fn permit_window_creation(app: &tauri::AppHandle) -> crate::error::BridgeResult<WindowPermit> {
    let state = app.state::<crate::AppState>();
    let mut gate = state
        .lifecycle_gate
        .lock()
        .map_err(|_| crate::error::BridgeError::host_state())?;
    if gate.active.is_some() {
        return Err(crate::error::BridgeError::new(
            "lifecycle-busy",
            "OpenPush is finishing a quit or account change.",
        ));
    }
    gate.creators += 1;
    Ok(WindowPermit { app: app.clone() })
}

pub struct SwitchPermit {
    app: tauri::AppHandle,
    id: String,
    labels: BTreeSet<String>,
    armed: bool,
}

impl SwitchPermit {
    pub async fn finish(mut self) -> crate::error::BridgeResult<()> {
        crate::heads_runtime::reset(&self.app).await?;
        for label in &self.labels {
            if crate::tray::composer_conversation(label).is_some() {
                if let Some(window) = self.app.get_webview_window(label) {
                    window.destroy().map_err(|_| {
                        crate::error::BridgeError::new(
                            "window",
                            "Could not close a prior-account composer window.",
                        )
                    })?;
                }
            }
        }
        let _ = release_topology(&self.app, &self.id);
        emit_finished(&self.app, &self.labels, &self.id, true);
        self.armed = false;
        Ok(())
    }
}

impl Drop for SwitchPermit {
    fn drop(&mut self) {
        if self.armed && release_topology(&self.app, &self.id) {
            emit_finished(&self.app, &self.labels, &self.id, false);
        }
    }
}

fn reserve_topology(
    app: &tauri::AppHandle,
    id: String,
    operation: Operation,
) -> crate::error::BridgeResult<()> {
    app.state::<crate::AppState>()
        .lifecycle_gate
        .lock()
        .map_err(|_| crate::error::BridgeError::host_state())?
        .reserve(id, operation)
        .map_err(|message| crate::error::BridgeError::new("lifecycle-busy", message))
}

fn release_topology(app: &tauri::AppHandle, id: &str) -> bool {
    app.state::<crate::AppState>()
        .lifecycle_gate
        .lock()
        .ok()
        .is_some_and(|mut gate| gate.release(id))
}

fn emit_finished(app: &tauri::AppHandle, labels: &BTreeSet<String>, id: &str, ok: bool) {
    let payload = Finished {
        id: id.to_owned(),
        ok,
    };
    for label in labels {
        let _ = app.emit_to(
            EventTarget::webview_window(label),
            FINISHED_EVENT,
            payload.clone(),
        );
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);

/// Sends a request to exactly one actual webview. Used by native head callbacks
/// as well as composer close handling; a window label is never client input.
pub fn request_window(
    app: &tauri::AppHandle,
    label: &str,
    action: Action,
) -> crate::error::BridgeResult<()> {
    let _ = begin(app, Operation::Window(action), [label.to_owned()])?;
    Ok(())
}

/// Waits for the webview's draft-save acknowledgement. Native application of
/// the action runs on the main thread before subsequently queued UI work;
/// the acknowledgement alone does not guarantee that window destruction succeeds.
pub(crate) async fn request_window_and_wait(
    app: &tauri::AppHandle,
    label: &str,
    action: Action,
) -> crate::error::BridgeResult<()> {
    let started = begin(app, Operation::Window(action), [label.to_owned()])?;
    started
        .done
        .await
        .map_err(|_| crate::error::BridgeError::host_state())?
        .map_err(|failure| crate::error::BridgeError::new("lifecycle", failure.message()))?;
    Ok(())
}

pub fn request_quit(app: &tauri::AppHandle) -> crate::error::BridgeResult<()> {
    let state = app.state::<crate::AppState>();
    if state
        .quit_requested
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return Ok(());
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Err(error) = reserve_topology(app, id.clone(), Operation::Quit) {
        state
            .quit_requested
            .store(false, std::sync::atomic::Ordering::Release);
        return Err(error);
    }
    let labels = app.webview_windows().keys().cloned().collect::<Vec<_>>();
    let started = match begin_with_id(app, Operation::Quit, labels, id.clone()) {
        Ok(started) => started,
        Err(error) => {
            let _ = release_topology(app, &id);
            state
                .quit_requested
                .store(false, std::sync::atomic::Ordering::Release);
            return Err(error);
        }
    };
    if started.request.labels.is_empty() {
        finish_quit(app);
    }
    Ok(())
}

/// Acquires a topology permit before target capture and keeps it through the
/// caller's actual config/session mutation. Dropping an unfinished permit
/// safely releases the gate and unfreezes every surviving target.
pub async fn prepare_switch(app: &tauri::AppHandle) -> crate::error::BridgeResult<SwitchPermit> {
    let id = uuid::Uuid::new_v4().to_string();
    reserve_topology(app, id.clone(), Operation::Switch)?;
    let labels = app.webview_windows().keys().cloned().collect::<Vec<_>>();
    let started = match begin_with_id(app, Operation::Switch, labels, id.clone()) {
        Ok(started) => started,
        Err(error) => {
            let _ = release_topology(app, &id);
            return Err(error);
        }
    };
    let request_labels = started.request.labels.clone();
    let completion = started
        .done
        .await
        .map_err(|_| {
            if release_topology(app, &id) {
                emit_finished(app, &request_labels, &id, false);
            }
            crate::error::BridgeError::host_state()
        })?
        .map_err(|failure| crate::error::BridgeError::new("lifecycle", failure.message()));
    match completion {
        Ok(completion) => Ok(SwitchPermit {
            app: app.clone(),
            id,
            labels: completion.labels,
            armed: true,
        }),
        Err(error) => {
            if release_topology(app, &id) {
                emit_finished(app, &request_labels, &id, false);
            }
            Err(error)
        }
    }
}

fn begin(
    app: &tauri::AppHandle,
    operation: Operation,
    labels: impl IntoIterator<Item = String>,
) -> crate::error::BridgeResult<Started> {
    begin_with_id(app, operation, labels, uuid::Uuid::new_v4().to_string())
}

fn begin_with_id(
    app: &tauri::AppHandle,
    operation: Operation,
    labels: impl IntoIterator<Item = String>,
    id: String,
) -> crate::error::BridgeResult<Started> {
    let started = {
        let state = app.state::<crate::AppState>();
        let mut coordinator = state
            .lifecycle
            .lock()
            .map_err(|_| crate::error::BridgeError::host_state())?;
        coordinator
            .begin_at_with_id(operation, labels, TIMEOUT, Instant::now(), id)
            .map_err(|message| crate::error::BridgeError::new("lifecycle-busy", message))?
    };

    for label in &started.request.labels {
        let emitted = app.get_webview_window(label).is_some()
            && app
                .emit_to(
                    EventTarget::webview_window(label),
                    REQUEST_EVENT,
                    started.request.clone(),
                )
                .is_ok();
        if !emitted {
            let failure = app
                .state::<crate::AppState>()
                .lifecycle
                .lock()
                .map_err(|_| crate::error::BridgeError::host_state())?
                .cancel(
                    &started.request.id,
                    FailureKind::EmitFailed,
                    Some(label.clone()),
                );
            if let Some(failure) = failure {
                restore_after_failure(app, &failure);
            }
            return Err(crate::error::BridgeError::new(
                "lifecycle",
                "Could not request draft saving.",
            ));
        }
    }

    if !started.request.labels.is_empty() {
        let handle = app.clone();
        let id = started.request.id.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(TIMEOUT).await;
            let failure = {
                let state = handle.state::<crate::AppState>();
                let mut coordinator = match state.lifecycle.lock() {
                    Ok(coordinator) => coordinator,
                    Err(poisoned) => poisoned.into_inner(),
                };
                coordinator.expire(&id, Instant::now())
            };
            if let Some(failure) = failure {
                restore_after_failure(&handle, &failure);
            }
        });
    }
    Ok(started)
}

pub fn acknowledge(
    app: &tauri::AppHandle,
    label: &str,
    id: &str,
    ok: bool,
) -> crate::error::BridgeResult<()> {
    let progress = {
        let state = app.state::<crate::AppState>();
        let mut coordinator = state
            .lifecycle
            .lock()
            .map_err(|_| crate::error::BridgeError::host_state())?;
        coordinator
            .acknowledge_at(id, label, ok, Instant::now())
            .map_err(|message| crate::error::BridgeError::new("lifecycle", message))?
    };
    match progress {
        Progress::Pending => Ok(()),
        Progress::Complete(completion) => {
            if let Err(error) = apply_completion(app, completion.clone()) {
                let failure = Failure {
                    operation: completion.operation,
                    request_id: id.to_owned(),
                    restore_labels: completion.labels.clone(),
                    labels: completion.labels,
                    failed_label: Some(label.to_owned()),
                    kind: FailureKind::ApplyFailed,
                };
                restore_after_failure(app, &failure);
                Err(error)
            } else {
                if matches!(completion.operation, Operation::Window(_)) {
                    emit_finished(app, &completion.labels, id, true);
                }
                Ok(())
            }
        }
        Progress::Failed(failure) => {
            restore_after_failure(app, &failure);
            Ok(())
        }
    }
}

fn apply_completion(
    app: &tauri::AppHandle,
    completion: Completion,
) -> crate::error::BridgeResult<()> {
    match completion.operation {
        Operation::Quit => finish_quit(app),
        Operation::Switch => {
            // The waiter owns account-switch teardown so callers cannot mutate
            // binding state until it has observed successful completion.
        }
        Operation::Window(action) => {
            for label in completion.labels {
                let Some(window) = app.get_webview_window(&label) else {
                    continue;
                };
                if label == crate::tray::MAIN {
                    crate::startup::background_main(app)?;
                } else {
                    match action {
                        Action::Close => window.destroy().map_err(|_| {
                            crate::error::BridgeError::new(
                                "window",
                                "Could not close the composer window.",
                            )
                        })?,
                        Action::Collapse => window.hide().map_err(|_| {
                            crate::error::BridgeError::new(
                                "window",
                                "Could not collapse the conversation panel.",
                            )
                        })?,
                        Action::Quit => unreachable!("quit uses its own operation"),
                    }
                }
            }
        }
    }
    Ok(())
}

fn finish_quit(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        handle.state::<crate::AppState>().close_session().await;
        crate::heads_runtime::clear_on_exit(&handle);
        let state = handle.state::<crate::AppState>();
        state
            .allow_exit
            .store(true, std::sync::atomic::Ordering::Release);
        handle.exit(0);
    });
}

fn restore_after_failure(app: &tauri::AppHandle, failure: &Failure) {
    if failure.operation == Operation::Quit {
        app.state::<crate::AppState>()
            .quit_requested
            .store(false, std::sync::atomic::Ordering::Release);
    }
    let _ = release_topology(app, &failure.request_id);
    let mut restored = false;
    for label in &failure.restore_labels {
        if label == crate::tray::MAIN {
            crate::tray::show_main(app);
            restored = true;
        } else if let Some(window) = app.get_webview_window(label) {
            let _ = window.show();
            let _ = window.set_focus();
            restored = true;
        }
    }
    if !restored {
        crate::tray::show_main(app);
    }
    emit_finished(app, &failure.labels, &failure.request_id, false);
    crate::dialogs::inform(app, "Drafts were not closed", failure.message());
    let _ = app.emit(crate::STATE_EVENT, ());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(milliseconds: u64) -> Instant {
        Instant::now() + Duration::from_millis(milliseconds)
    }

    #[tokio::test]
    async fn completion_preserves_original_targets_and_operation() {
        let now = at(0);
        let mut coordinator = Coordinator::default();
        let mut started = coordinator
            .begin_at(
                Operation::Window(Action::Close),
                ["composer-a".to_owned(), "composer-b".to_owned()],
                Duration::from_secs(1),
                now,
            )
            .unwrap();

        assert_eq!(
            coordinator
                .acknowledge_at(&started.request.id, "composer-b", true, now)
                .unwrap(),
            Progress::Pending
        );
        assert_eq!(
            started.done.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        let Progress::Complete(completion) = coordinator
            .acknowledge_at(&started.request.id, "composer-a", true, now)
            .unwrap()
        else {
            panic!("the final acknowledgement must complete the transaction");
        };
        assert_eq!(completion.operation, Operation::Window(Action::Close));
        assert_eq!(
            completion.labels,
            BTreeSet::from(["composer-a".to_owned(), "composer-b".to_owned()])
        );
        assert_eq!(started.done.await.unwrap().unwrap(), completion);
    }

    #[tokio::test]
    async fn failure_aborts_waiter_without_waiting_for_other_windows() {
        let now = at(0);
        let mut coordinator = Coordinator::default();
        let started = coordinator
            .begin_at(
                Operation::Switch,
                ["main".to_owned(), "composer-a".to_owned()],
                Duration::from_secs(1),
                now,
            )
            .unwrap();

        let Progress::Failed(failure) = coordinator
            .acknowledge_at(&started.request.id, "composer-a", false, now)
            .unwrap()
        else {
            panic!("a negative acknowledgement must abort the transaction");
        };
        assert_eq!(failure.kind, FailureKind::Rejected);
        assert_eq!(failure.failed_label.as_deref(), Some("composer-a"));
        assert_eq!(started.done.await.unwrap().unwrap_err(), failure);
        assert!(!coordinator.is_pending());
    }

    #[tokio::test]
    async fn overdue_ack_is_rejected_before_timer_wakes() {
        let now = at(0);
        let mut coordinator = Coordinator::default();
        let started = coordinator
            .begin_at(
                Operation::Quit,
                ["main".to_owned()],
                Duration::from_millis(10),
                now,
            )
            .unwrap();

        let Progress::Failed(failure) = coordinator
            .acknowledge_at(
                &started.request.id,
                "main",
                true,
                now + Duration::from_millis(11),
            )
            .unwrap()
        else {
            panic!("an overdue acknowledgement must fail the transaction");
        };
        assert_eq!(failure.kind, FailureKind::TimedOut);
        assert_eq!(started.done.await.unwrap().unwrap_err(), failure);
    }

    #[tokio::test]
    async fn stale_timer_cannot_expire_a_new_request() {
        let now = at(0);
        let mut coordinator = Coordinator::default();
        let first = coordinator
            .begin_at(
                Operation::Window(Action::Close),
                ["main".to_owned()],
                Duration::from_millis(10),
                now,
            )
            .unwrap();
        coordinator.cancel(&first.request.id, FailureKind::EmitFailed, None);
        let second = coordinator
            .begin_at(
                Operation::Quit,
                ["main".to_owned()],
                Duration::from_secs(1),
                now,
            )
            .unwrap();

        assert!(coordinator
            .expire(&first.request.id, now + Duration::from_millis(11))
            .is_none());
        assert!(coordinator.is_pending());
        assert_eq!(
            coordinator
                .acknowledge_at(&second.request.id, "main", true, now)
                .unwrap(),
            Progress::Complete(Completion {
                operation: Operation::Quit,
                labels: BTreeSet::from(["main".to_owned()]),
            })
        );
    }

    #[test]
    fn rejects_busy_unknown_and_duplicate_acknowledgements() {
        let now = at(0);
        let mut coordinator = Coordinator::default();
        let started = coordinator
            .begin_at(
                Operation::Quit,
                ["main".to_owned(), "composer-a".to_owned()],
                Duration::from_secs(1),
                now,
            )
            .unwrap();
        assert!(coordinator
            .begin_at(
                Operation::Quit,
                ["main".to_owned()],
                Duration::from_secs(1),
                now,
            )
            .is_err());
        assert!(coordinator
            .acknowledge_at("old", "main", true, now)
            .is_err());
        assert!(coordinator
            .acknowledge_at(&started.request.id, "unknown", true, now)
            .is_err());
        assert_eq!(
            coordinator
                .acknowledge_at(&started.request.id, "main", true, now)
                .unwrap(),
            Progress::Pending
        );
        assert!(coordinator
            .acknowledge_at(&started.request.id, "main", true, now)
            .is_err());
    }

    #[tokio::test]
    async fn emit_failure_cancels_the_pending_transaction() {
        let now = at(0);
        let mut coordinator = Coordinator::default();
        let started = coordinator
            .begin_at(
                Operation::Window(Action::Close),
                ["composer-a".to_owned()],
                TIMEOUT,
                now,
            )
            .unwrap();
        let failure = coordinator
            .cancel(
                &started.request.id,
                FailureKind::EmitFailed,
                Some("composer-a".to_owned()),
            )
            .unwrap();
        assert_eq!(failure.kind, FailureKind::EmitFailed);
        assert_eq!(started.done.await.unwrap().unwrap_err(), failure);
        assert!(!coordinator.is_pending());
    }

    #[test]
    fn failure_restores_only_unacknowledged_targets() {
        let now = at(0);
        let mut coordinator = Coordinator::default();
        let started = coordinator
            .begin_at(
                Operation::Quit,
                ["main".to_owned(), "composer-a".to_owned()],
                Duration::from_secs(1),
                now,
            )
            .unwrap();
        assert_eq!(
            coordinator
                .acknowledge_at(&started.request.id, "main", true, now)
                .unwrap(),
            Progress::Pending
        );
        let Progress::Failed(failure) = coordinator
            .acknowledge_at(&started.request.id, "composer-a", false, now)
            .unwrap()
        else {
            panic!("negative acknowledgement must fail");
        };
        assert_eq!(failure.labels.len(), 2);
        assert_eq!(
            failure.restore_labels,
            BTreeSet::from(["composer-a".to_owned()])
        );
    }

    #[test]
    fn topology_gate_rejects_switch_during_window_creation_and_new_windows_during_switch() {
        let mut gate = TopologyGate {
            creators: 1,
            ..Default::default()
        };
        assert!(gate.reserve("switch".into(), Operation::Switch).is_err());
        gate.creators = 0;
        gate.reserve("switch".into(), Operation::Switch).unwrap();
        assert!(gate.blocked());
        assert!(gate.reserve("quit".into(), Operation::Quit).is_err());
        assert!(gate.release("switch"));
        assert!(!gate.blocked());
    }

    #[test]
    fn native_close_policy_distinguishes_main_panel_and_composer() {
        assert_eq!(
            close_operation(true, true, false),
            Operation::Window(Action::Close)
        );
        assert_eq!(close_operation(true, false, false), Operation::Quit);
        assert_eq!(
            close_operation(false, true, true),
            Operation::Window(Action::Collapse)
        );
        assert_eq!(
            close_operation(false, true, false),
            Operation::Window(Action::Close)
        );
    }

    #[tokio::test]
    async fn empty_switch_completes_immediately() {
        let now = at(0);
        let mut coordinator = Coordinator::default();
        let started = coordinator
            .begin_at(Operation::Switch, Vec::<String>::new(), TIMEOUT, now)
            .unwrap();
        assert_eq!(
            started.done.await.unwrap().unwrap(),
            Completion {
                operation: Operation::Switch,
                labels: BTreeSet::new(),
            }
        );
        assert!(!coordinator.is_pending());
    }
}
