//! Per-desktop notification policy. This module deliberately owns no synchronized content: core
//! supplies live-only banner candidates and the UI receives the sanitized snapshot separately.
use crate::{
    error::{BridgeError, BridgeResult},
    fsutil,
    session::Session,
    tray,
};
use openpush_client_core::BannerCandidate;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

const PREFERENCES_FILE: &str = "notification-preferences.json";
const MAX_PREFERENCES_BYTES: u64 = 8 * 1024;
const BANNER_BATCH: usize = 20;
const MAX_BANNER_BATCHES_PER_WAKE: usize = 5;
const BANNER_BURST_SUMMARY_THRESHOLD: usize = 3;
const STALE_CANDIDATE_MS: i64 = 2 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPreferences {
    pub message_banners: bool,
    pub mirrored_banners: bool,
    pub preview: Preview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preview {
    Full,
    Hidden,
}

impl Default for NotificationPreferences {
    fn default() -> Self {
        Self {
            message_banners: true,
            mirrored_banners: true,
            preview: Preview::Full,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationView {
    Conversations,
    Notifications,
    Settings,
}

#[derive(Default)]
pub struct NotificationContext {
    pub view: Option<NotificationView>,
    pub conversation_id: Option<String>,
}

pub struct NotificationSettings {
    path: PathBuf,
    preferences: Mutex<NotificationPreferences>,
    pub context: Mutex<NotificationContext>,
    /// Serializes core queue read/post/ack without holding the application/session mutex.
    drain_lock: Mutex<()>,
}

impl NotificationSettings {
    pub fn load(root: &Path) -> Self {
        let path = root.join(PREFERENCES_FILE);
        let preferences = fs::metadata(&path)
            .ok()
            .filter(|metadata| metadata.len() <= MAX_PREFERENCES_BYTES)
            .and_then(|_| fs::read(&path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            preferences: Mutex::new(preferences),
            context: Mutex::new(NotificationContext::default()),
            drain_lock: Mutex::new(()),
        }
    }

    pub fn preferences(&self) -> NotificationPreferences {
        self.preferences
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn set_preferences(&self, preferences: NotificationPreferences) -> BridgeResult<()> {
        let mut write = self.preferences.lock().unwrap_or_else(|p| p.into_inner());
        let bytes = serde_json::to_vec(&preferences).map_err(|_| {
            BridgeError::new(
                "notification-preferences",
                "Could not save notification preferences.",
            )
        })?;
        fsutil::write_private_atomic(&self.path, &bytes).map_err(|_| {
            BridgeError::new(
                "notification-preferences",
                "Could not save notification preferences.",
            )
        })?;
        *write = preferences;
        Ok(())
    }

    pub fn set_context(&self, view: NotificationView, conversation_id: Option<String>) {
        *self.context.lock().unwrap_or_else(|p| p.into_inner()) = NotificationContext {
            view: Some(view),
            conversation_id,
        };
    }
}

fn stable_id(candidate: &BannerCandidate) -> i32 {
    let mut hash = 0x811c9dc5u32;
    for byte in candidate.id.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x01000193);
    }
    (hash & 0x7fff_ffff) as i32
}

fn focused_for_candidate(
    app: &AppHandle,
    settings: &NotificationSettings,
    candidate: &BannerCandidate,
) -> bool {
    let main_focused = app
        .get_webview_window(tray::MAIN)
        .and_then(|window| window.is_focused().ok())
        .unwrap_or(false);
    let context = settings.context.lock().unwrap_or_else(|p| p.into_inner());
    if should_suppress_main_focus(main_focused, &context, candidate) {
        return true;
    }
    candidate
        .conversation_id
        .as_ref()
        .is_some_and(|conversation| {
            app.get_webview_window(&format!("{}{}", tray::COMPOSER_PREFIX, conversation))
                .and_then(|window| window.is_focused().ok())
                .unwrap_or(false)
        })
}

fn should_suppress_main_focus(
    main_focused: bool,
    context: &NotificationContext,
    candidate: &BannerCandidate,
) -> bool {
    if !main_focused {
        return false;
    }
    match context.view {
        Some(NotificationView::Notifications) => candidate.kind == "notification",
        Some(NotificationView::Conversations) => context
            .conversation_id
            .as_deref()
            .zip(candidate.conversation_id.as_deref())
            .is_some_and(|(active, candidate)| active == candidate),
        _ => false,
    }
}

fn allowed(preferences: &NotificationPreferences, candidate: &BannerCandidate) -> bool {
    match candidate.kind.as_str() {
        "message" => preferences.message_banners,
        "notification" => preferences.mirrored_banners,
        _ => false,
    }
}

fn banner_text(
    preferences: &NotificationPreferences,
    candidate: &BannerCandidate,
) -> (String, String) {
    match preferences.preview {
        Preview::Full => (candidate.title.clone(), candidate.body.clone()),
        Preview::Hidden => match candidate.kind.as_str() {
            "message" => ("New message".into(), "Open OpenPush to view it.".into()),
            _ => (
                "New notification".into(),
                "Open OpenPush to view it.".into(),
            ),
        },
    }
}

/// Drains only durable live candidates produced by core. A candidate is acknowledged after an OS
/// post attempt (or intentional policy suppression), leaving the unavoidable crash window after
/// delivery and before acknowledgement explicit. Snapshot/history imports never create these rows.
pub fn drain_banner_candidates(
    app: &AppHandle,
    session: &Session,
    settings: &NotificationSettings,
) -> BridgeResult<bool> {
    let _drain = settings
        .drain_lock
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let preferences = settings.preferences();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(i64::MAX);
    let mut more = false;
    for _ in 0..MAX_BANNER_BATCHES_PER_WAKE {
        let candidates = session
            .client
            .pending_banner_candidates(BANNER_BATCH)
            .map_err(crate::error::core_error)?;
        if candidates.is_empty() {
            break;
        }
        let mut acknowledge = Vec::with_capacity(candidates.len());
        let mut summary_count = 0usize;
        let mut recent = Vec::new();
        for candidate in candidates {
            if !allowed(&preferences, &candidate)
                || focused_for_candidate(app, settings, &candidate)
            {
                acknowledge.push(candidate.id);
            } else if now.saturating_sub(candidate.created_at) > STALE_CANDIDATE_MS {
                summary_count += 1;
                acknowledge.push(candidate.id);
            } else {
                recent.push(candidate);
            }
        }
        if recent.len() > BANNER_BURST_SUMMARY_THRESHOLD {
            summary_count += recent.len();
            acknowledge.extend(recent.into_iter().map(|candidate| candidate.id));
        } else {
            for candidate in recent {
                let (title, body) = banner_text(&preferences, &candidate);
                if app
                    .notification()
                    .builder()
                    .id(stable_id(&candidate))
                    .title(title)
                    .body(body)
                    .show()
                    .is_err()
                {
                    if !acknowledge.is_empty() {
                        session
                            .client
                            .ack_banner_candidates(acknowledge)
                            .map_err(crate::error::core_error)?;
                    }
                    session.set_status(|status| status.work_error = Some("notification-post"));
                    return Err(BridgeError::new(
                        "notification-post",
                        "Could not post a native notification.",
                    ));
                }
                acknowledge.push(candidate.id);
            }
        }
        if summary_count > 0 && (preferences.message_banners || preferences.mirrored_banners) {
            if app
                .notification()
                .builder()
                .id(0x4f50_5553)
                .title("OpenPush")
                .body(format!(
                    "{summary_count} new items arrived while OpenPush was away."
                ))
                .show()
                .is_err()
            {
                if !acknowledge.is_empty() {
                    session
                        .client
                        .ack_banner_candidates(acknowledge)
                        .map_err(crate::error::core_error)?;
                }
                session.set_status(|status| status.work_error = Some("notification-post"));
                return Err(BridgeError::new(
                    "notification-post",
                    "Could not post a native notification.",
                ));
            }
        }
        session
            .client
            .ack_banner_candidates(acknowledge)
            .map_err(crate::error::core_error)?;
        more = session
            .client
            .pending_banner_candidates(1)
            .map_err(crate::error::core_error)?
            .len()
            == 1;
        if !more {
            break;
        }
    }
    if !more {
        session.set_status(|status| {
            if status.work_error == Some("notification-post") {
                status.work_error = None;
            }
        });
    }
    Ok(more)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_default_and_persist_privately() {
        let dir = tempfile::tempdir().unwrap();
        let settings = NotificationSettings::load(dir.path());
        assert_eq!(settings.preferences(), NotificationPreferences::default());
        settings
            .set_preferences(NotificationPreferences {
                message_banners: false,
                mirrored_banners: true,
                preview: Preview::Hidden,
            })
            .unwrap();
        assert_eq!(
            NotificationSettings::load(dir.path()).preferences().preview,
            Preview::Hidden
        );
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &fs::metadata(dir.path().join(PREFERENCES_FILE))
                    .unwrap()
                    .permissions()
            ) & 0o777,
            0o600
        );
    }

    #[test]
    fn hidden_preview_never_uses_candidate_content() {
        let candidate = BannerCandidate {
            id: "candidate".into(),
            kind: "notification".into(),
            conversation_id: None,
            notification_target: None,
            title: "private title".into(),
            body: "private body".into(),
            created_at: 0,
        };
        let rendered = banner_text(
            &NotificationPreferences {
                preview: Preview::Hidden,
                ..NotificationPreferences::default()
            },
            &candidate,
        );
        assert!(!rendered.0.contains("private"));
        assert!(!rendered.1.contains("private"));
        assert!(allowed(&NotificationPreferences::default(), &candidate));
    }

    #[test]
    fn focus_policy_only_suppresses_matching_content() {
        let message = BannerCandidate {
            id: "message".into(),
            kind: "message".into(),
            conversation_id: Some("conversation-a".into()),
            notification_target: None,
            title: String::new(),
            body: String::new(),
            created_at: 0,
        };
        let mirrored = BannerCandidate {
            id: "mirror".into(),
            kind: "notification".into(),
            conversation_id: None,
            notification_target: None,
            title: String::new(),
            body: String::new(),
            created_at: 0,
        };
        let notifications = NotificationContext {
            view: Some(NotificationView::Notifications),
            conversation_id: None,
        };
        assert!(should_suppress_main_focus(true, &notifications, &mirrored));
        assert!(!should_suppress_main_focus(true, &notifications, &message));
        let conversations = NotificationContext {
            view: Some(NotificationView::Conversations),
            conversation_id: Some("conversation-a".into()),
        };
        assert!(should_suppress_main_focus(true, &conversations, &message));
        assert!(!should_suppress_main_focus(true, &conversations, &mirrored));
    }

    #[test]
    fn finite_wake_bounds_burst_processing() {
        assert_eq!(BANNER_BATCH * MAX_BANNER_BATCHES_PER_WAKE, 100);
        assert!(BANNER_BATCH >= 20);
        assert!(BANNER_BURST_SUMMARY_THRESHOLD < BANNER_BATCH);
    }
}
