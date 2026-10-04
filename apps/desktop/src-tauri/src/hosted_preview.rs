//! Debug-only native host for the hosted onboarding reducer. It deliberately exposes public
//! reducer facts only: passphrases remain in the OS dialog and are dropped before persistence.
use crate::{
    dialogs::{self, PassphrasePurpose},
    error::{BridgeError, BridgeResult},
    fsutil, require_main,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Manager, State, WebviewWindow};
use zeroize::Zeroizing;

const CHECKPOINT_NAME: &str = "hosted-preview.json";
#[cfg(test)]
const CANARY: &str = "canary-zebra-violet-mountain-ledger";

pub struct HostedPreviewState {
    lock: tokio::sync::Mutex<PreviewSession>,
}

struct PreviewSession {
    provision_retry_failed: bool,
}

impl Default for HostedPreviewState {
    fn default() -> Self {
        Self {
            lock: tokio::sync::Mutex::new(PreviewSession {
                provision_retry_failed: false,
            }),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Checkpoint {
    scenario: String,
    screen: String,
    account_state: String,
    entitlement_state: String,
    provider: String,
    operation_id: Option<String>,
    status_key: Option<String>,
    approval_state: String,
    unlocked: bool,
    rejected: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostedPreviewView {
    scenario: String,
    screen: String,
    account_state: String,
    entitlement_state: String,
    approval_state: String,
    unlocked: bool,
    rejected: bool,
    status_key: Option<String>,
    local_error: Option<&'static str>,
    operation_id: Option<String>,
    fixture: HostedPreviewFixture,
    scenarios: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostedPreviewFixture {
    account_label: Option<&'static str>,
    sign_in_provider: Option<&'static str>,
    subscription: Option<HostedPreviewSubscription>,
    approval_code: Option<&'static str>,
    hosted_origin: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostedPreviewSubscription {
    display_price: &'static str,
    status: String,
}

fn preview_error(code: &'static str, message: &'static str) -> BridgeError {
    BridgeError::new(code, message)
}

fn native_only_event(event: &str) -> bool {
    matches!(event, "local_passphrase_accepted" | "passphrase_confirmed")
}

fn checkpoint_path(app: &AppHandle) -> BridgeResult<PathBuf> {
    app.path()
        .app_data_dir()
        .map(|root| root.join(CHECKPOINT_NAME))
        .map_err(|_| preview_error("host-state", "Native app data is unavailable."))
}

fn checkpoint(snapshot: &str) -> Option<Checkpoint> {
    serde_json::from_str::<serde_json::Value>(snapshot)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn view(snapshot: &str, local_error: Option<&'static str>) -> BridgeResult<HostedPreviewView> {
    let value = checkpoint(snapshot)
        .ok_or_else(|| preview_error("host-state", "Preview state is invalid."))?;
    let signed_in = value.account_state != "anonymous";
    let store_unavailable =
        value.status_key.as_deref() == Some("hosted_subscribe_store_unavailable");
    let paid = matches!(
        value.entitlement_state.as_str(),
        "active" | "grace" | "billing_retry"
    );
    Ok(HostedPreviewView {
        scenario: value.scenario.clone(),
        screen: value.screen.clone(),
        account_state: value.account_state,
        entitlement_state: value.entitlement_state.clone(),
        approval_state: value.approval_state,
        unlocked: value.unlocked,
        rejected: value.rejected,
        status_key: value.status_key,
        local_error,
        operation_id: value.operation_id,
        fixture: HostedPreviewFixture {
            account_label: signed_in.then_some("Preview account"),
            sign_in_provider: None,
            subscription: (signed_in && !store_unavailable).then_some(HostedPreviewSubscription {
                display_price: "$4.99/month · Preview price",
                status: value.entitlement_state,
            }),
            approval_code: matches!(value.screen.as_str(), "join" | "approval")
                .then_some("418 207"),
            hosted_origin: (signed_in && paid).then_some("preview.peppy.pro (preview)"),
        },
        scenarios: peppy_hosted_onboarding::SCENARIOS
            .iter()
            .map(|scenario| (*scenario).to_owned())
            .collect(),
    })
}

fn valid_resume(input: &str) -> Option<String> {
    let resumed = peppy_hosted_onboarding::resume(input);
    checkpoint(&resumed)
        .filter(|state| !state.rejected)
        .map(|_| resumed)
}

fn load(path: &Path) -> String {
    fs::read_to_string(path)
        .ok()
        .and_then(|input| valid_resume(&input))
        .unwrap_or_else(|| peppy_hosted_onboarding::start("new"))
}

fn persist(path: &Path, snapshot: &str) -> BridgeResult<()> {
    fsutil::write_private_atomic(path, snapshot.as_bytes())
        .map_err(|_| preview_error("host-state", "Preview state could not be saved."))
}

fn advance(snapshot: String, event: &str) -> String {
    peppy_hosted_onboarding::advance(&snapshot, event)
}

/// Mirrors the preview service's account-bound entitlement check without performing I/O.
fn preview_entitlement_valid(state: &Checkpoint) -> bool {
    let account = ("preview-account", "preview");
    let purchase = ("preview-account", "preview");
    state.account_state != "anonymous"
        && state.provider == account.1
        && purchase.0 == account.0
        && purchase.1 == account.1
}

fn validate_entitlement(snapshot: String) -> String {
    let Some(state) = checkpoint(&snapshot) else {
        return peppy_hosted_onboarding::start("new");
    };
    if state.entitlement_state != "verifying" {
        return snapshot;
    }
    advance(
        snapshot,
        if preview_entitlement_valid(&state) {
            "entitlement_verified"
        } else {
            "entitlement_rejected"
        },
    )
}

fn native_create(
    snapshot: String,
    passphrase: Option<Zeroizing<String>>,
) -> (String, Option<&'static str>) {
    let Some(passphrase) = passphrase else {
        return (snapshot, None);
    };
    let acceptable = peppy_hosted_onboarding::passphrase_acceptable(&passphrase);
    drop(passphrase);
    if !acceptable {
        return (snapshot, Some("passphrase_weak"));
    }
    if checkpoint(&snapshot).is_some_and(|state| state.screen == "confirm") {
        return (advance(snapshot, "passphrase_confirmed"), None);
    }
    let snapshot = advance(snapshot, "local_passphrase_accepted");
    (advance(snapshot, "passphrase_confirmed"), None)
}

fn native_unlock(
    snapshot: String,
    passphrase: Option<Zeroizing<String>>,
) -> (String, Option<&'static str>) {
    let Some(passphrase) = passphrase else {
        return (snapshot, None);
    };
    let non_empty = !passphrase.is_empty();
    drop(passphrase);
    if !non_empty {
        return (snapshot, Some("preview_error"));
    }
    (advance(snapshot, "passphrase_confirmed"), None)
}

fn ensure_screen(snapshot: &str, screens: &[&str]) -> BridgeResult<()> {
    match checkpoint(snapshot).filter(|state| screens.contains(&state.screen.as_str())) {
        Some(_) => Ok(()),
        None => Err(preview_error(
            "preview-wrong-screen",
            "This preview action is not available on the current screen.",
        )),
    }
}

fn composed_advance(snapshot: String, event: &str, retry_failed: &mut bool) -> String {
    let Some(state) = checkpoint(&snapshot) else {
        return peppy_hosted_onboarding::start("new");
    };
    match event {
        "signed_in" => advance(snapshot, "signed_in"),
        "purchase_succeeded" | "restore_succeeded"
            if matches!(
                state.entitlement_state.as_str(),
                "active" | "grace" | "billing_retry"
            ) =>
        {
            advance(snapshot, "restore_succeeded")
        }
        "purchase_succeeded" | "restore_succeeded" => {
            let snapshot = advance(snapshot, event);
            if checkpoint(&snapshot).is_none_or(|state| state.rejected) {
                return snapshot;
            }
            let snapshot = advance(snapshot, "verify_entitlement");
            validate_entitlement(snapshot)
        }
        "verify_entitlement" => {
            let snapshot = if state.entitlement_state == "store_succeeded_unverified" {
                advance(snapshot, "verify_entitlement")
            } else {
                snapshot
            };
            validate_entitlement(snapshot)
        }
        "provision_finished"
            if state.screen == "provisioning"
                && state.status_key.is_none()
                && state.operation_id.is_some() =>
        {
            if state.scenario == "provision_retry" && !*retry_failed {
                *retry_failed = true;
                advance(snapshot, "provision_failed")
            } else {
                advance(snapshot, "provision_finished")
            }
        }
        _ => advance(snapshot, event),
    }
}

#[tauri::command]
pub async fn hosted_preview_state(
    window: WebviewWindow,
    app: AppHandle,
    state: State<'_, HostedPreviewState>,
) -> BridgeResult<HostedPreviewView> {
    require_main(window.label())?;
    let _guard = state.lock.lock().await;
    let path = checkpoint_path(&app)?;
    let snapshot = load(&path);
    persist(&path, &snapshot)?;
    view(&snapshot, None)
}

#[tauri::command]
pub async fn hosted_preview_start(
    window: WebviewWindow,
    app: AppHandle,
    state: State<'_, HostedPreviewState>,
    scenario: String,
) -> BridgeResult<HostedPreviewView> {
    require_main(window.label())?;
    if !peppy_hosted_onboarding::SCENARIOS.contains(&scenario.as_str()) {
        return Err(preview_error(
            "preview-scenario",
            "Unknown preview scenario.",
        ));
    }
    let mut session = state.lock.lock().await;
    session.provision_retry_failed = false;
    let path = checkpoint_path(&app)?;
    let snapshot = peppy_hosted_onboarding::start(&scenario);
    persist(&path, &snapshot)?;
    view(&snapshot, None)
}

#[tauri::command]
pub async fn hosted_preview_advance(
    window: WebviewWindow,
    app: AppHandle,
    state: State<'_, HostedPreviewState>,
    event: String,
) -> BridgeResult<HostedPreviewView> {
    require_main(window.label())?;
    if native_only_event(&event) {
        return Err(preview_error(
            "preview-native-only",
            "This preview step uses a native secure dialog.",
        ));
    }
    let mut session = state.lock.lock().await;
    let path = checkpoint_path(&app)?;
    let snapshot = composed_advance(load(&path), &event, &mut session.provision_retry_failed);
    persist(&path, &snapshot)?;
    view(&snapshot, None)
}

#[tauri::command]
pub async fn hosted_preview_create_passphrase(
    window: WebviewWindow,
    app: AppHandle,
    state: State<'_, HostedPreviewState>,
) -> BridgeResult<HostedPreviewView> {
    require_main(window.label())?;
    let _guard = state.lock.lock().await;
    let path = checkpoint_path(&app)?;
    let current = load(&path);
    ensure_screen(&current, &["passphrase", "confirm"])?;
    let (snapshot, local_error) = native_create(
        current.clone(),
        dialogs::passphrase(&app, PassphrasePurpose::Create).await?,
    );
    if snapshot != current {
        persist(&path, &snapshot)?;
    }
    view(&snapshot, local_error)
}

#[tauri::command]
pub async fn hosted_preview_unlock(
    window: WebviewWindow,
    app: AppHandle,
    state: State<'_, HostedPreviewState>,
) -> BridgeResult<HostedPreviewView> {
    require_main(window.label())?;
    let _guard = state.lock.lock().await;
    let path = checkpoint_path(&app)?;
    let current = load(&path);
    ensure_screen(&current, &["unlock"])?;
    let (snapshot, local_error) = native_unlock(
        current.clone(),
        dialogs::passphrase(&app, PassphrasePurpose::Unlock).await?,
    );
    if snapshot != current {
        persist(&path, &snapshot)?;
    }
    view(&snapshot, local_error)
}

#[tauri::command]
pub async fn hosted_preview_reset(
    window: WebviewWindow,
    app: AppHandle,
    state: State<'_, HostedPreviewState>,
) -> BridgeResult<HostedPreviewView> {
    require_main(window.label())?;
    let mut session = state.lock.lock().await;
    session.provision_retry_failed = false;
    let path = checkpoint_path(&app)?;
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(preview_error(
                "host-state",
                "Preview state could not be reset.",
            ))
        }
    }
    view(&peppy_hosted_onboarding::start("new"), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn reach_passphrase() -> String {
        let state = advance(peppy_hosted_onboarding::start("new"), "hosted_start");
        let state = advance(state, "signed_in");
        let state = advance(state, "purchase_succeeded");
        let state = advance(state, "verify_entitlement");
        advance(state, "entitlement_verified")
    }

    fn acceptable_canary() -> String {
        format!("{CANARY} alpha bravo charlie delta")
    }

    #[test]
    fn create_cancel_weak_and_accept_are_native_only() {
        let state = reach_passphrase();
        assert_eq!(native_create(state.clone(), None), (state.clone(), None));
        assert_eq!(
            native_create(state.clone(), Some(Zeroizing::new("weak".into()))).1,
            Some("passphrase_weak")
        );
        let (state, error) = native_create(state, Some(Zeroizing::new(acceptable_canary())));
        assert_eq!(error, None);
        assert_eq!(checkpoint(&state).unwrap().screen, "provisioning");
        assert!(!state
            .as_bytes()
            .windows(CANARY.len())
            .any(|part| part == CANARY.as_bytes()));
    }

    #[test]
    fn unlock_cancel_empty_and_accept() {
        let state = advance(
            advance(peppy_hosted_onboarding::start("returning"), "hosted_start"),
            "signed_in",
        );
        let state = advance(state, "approval_granted");
        assert_eq!(native_unlock(state.clone(), None), (state.clone(), None));
        assert_eq!(
            native_unlock(state.clone(), Some(Zeroizing::new(String::new()))).1,
            Some("preview_error")
        );
        assert_eq!(
            checkpoint(&native_unlock(state, Some(Zeroizing::new(acceptable_canary()))).0)
                .unwrap()
                .screen,
            "permissions"
        );
    }

    #[test]
    fn checkpoint_loading_is_safe_and_view_is_sanitized() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(CHECKPOINT_NAME);
        fs::write(&path, b"not json").unwrap();
        let state = load(&path);
        assert_eq!(checkpoint(&state).unwrap().screen, "welcome");
        let (state, _) = native_create(
            reach_passphrase(),
            Some(Zeroizing::new(acceptable_canary())),
        );
        persist(&path, &state).unwrap();
        let encoded = serde_json::to_string(&view(&state, None).unwrap()).unwrap();
        assert!(!fs::read(&path)
            .unwrap()
            .windows(CANARY.len())
            .any(|part| part == CANARY.as_bytes()));
        assert!(!encoded.contains(CANARY));
    }

    #[test]
    fn denylist_unknown_scenario_paid_resume_retry_and_wrong_screen() {
        assert!(!peppy_hosted_onboarding::SCENARIOS.contains(&"unknown"));
        let state = reach_passphrase();
        assert!(native_only_event("local_passphrase_accepted"));
        let after = if native_only_event("local_passphrase_accepted") {
            state.clone()
        } else {
            advance(state.clone(), "local_passphrase_accepted")
        };
        assert_eq!(after, state);
        assert!(ensure_screen(&state, &["unlock"]).is_err());
        let paid = advance(
            advance(peppy_hosted_onboarding::start("returning"), "hosted_start"),
            "signed_in",
        );
        let mut failed = false;
        assert_eq!(
            checkpoint(&composed_advance(paid, "restore_succeeded", &mut failed))
                .unwrap()
                .screen,
            "join"
        );
        let provisioning = advance(
            advance(
                peppy_hosted_onboarding::start("provision_retry"),
                "hosted_start",
            ),
            "signed_in",
        );
        let first = composed_advance(
            advance(provisioning, "provision_retry"),
            "provision_finished",
            &mut failed,
        );
        assert_eq!(
            checkpoint(&first).unwrap().status_key.as_deref(),
            Some("provisioning_error")
        );
        let second = composed_advance(
            advance(first, "provision_retry"),
            "provision_finished",
            &mut failed,
        );
        assert_eq!(checkpoint(&second).unwrap().screen, "permissions");
    }

    #[test]
    fn store_retry_restores_subscription_and_purchase_reaches_passphrase() {
        let mut retry_failed = false;
        let state = advance(
            peppy_hosted_onboarding::start("store_unavailable"),
            "hosted_start",
        );
        let state = composed_advance(state, "signed_in", &mut retry_failed);
        assert!(view(&state, None).unwrap().fixture.subscription.is_none());
        let state = composed_advance(state, "store_retry", &mut retry_failed);
        assert!(view(&state, None).unwrap().fixture.subscription.is_some());
        let state = composed_advance(state, "purchase_succeeded", &mut retry_failed);
        assert_eq!(checkpoint(&state).unwrap().screen, "passphrase");
    }

    #[test]
    fn resumed_verification_and_returning_unlock_flow_complete() {
        let mut retry_failed = false;
        let verifying = advance(
            advance(
                advance(peppy_hosted_onboarding::start("new"), "hosted_start"),
                "signed_in",
            ),
            "purchase_succeeded",
        );
        let resumed = peppy_hosted_onboarding::resume(&verifying);
        let state = composed_advance(resumed, "verify_entitlement", &mut retry_failed);
        assert_eq!(checkpoint(&state).unwrap().screen, "passphrase");

        let state = composed_advance(
            advance(peppy_hosted_onboarding::start("returning"), "hosted_start"),
            "signed_in",
            &mut retry_failed,
        );
        let state = composed_advance(state, "approval_granted", &mut retry_failed);
        let (state, error) = native_unlock(state, Some(Zeroizing::new(acceptable_canary())));
        assert_eq!(error, None);
        let state = composed_advance(state, "permissions_done", &mut retry_failed);
        assert_eq!(checkpoint(&state).unwrap().screen, "settings");
    }
}
