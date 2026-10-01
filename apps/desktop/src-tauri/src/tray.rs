//! Tray/background behavior and composer windows. When a tray icon is available, closing the
//! main window hides it and sync continues in the background; when the platform cannot provide
//! a tray, closing quits normally so the app is never silently trapped. Incoming updates never
//! create, show or focus a window; only explicit user actions do.
use crate::error::{BridgeError, BridgeResult};
use openpush_client_core::ConversationId;
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};

pub const MAIN: &str = "main";
pub const COMPOSER_PREFIX: &str = "composer-";

/// Builds the tray; returns false when the platform/session offers no tray.
pub fn install(app: &AppHandle, on_compose: impl Fn(&AppHandle) + Send + Sync + 'static) -> bool {
    let build = || -> tauri::Result<()> {
        let open = MenuItem::with_id(app, "open", "Open OpenPush", true, None::<&str>)?;
        let compose = MenuItem::with_id(app, "compose", "New message", true, None::<&str>)?;
        let separator = PredefinedMenuItem::separator(app)?;
        let quit = MenuItem::with_id(app, "quit", "Quit OpenPush", true, None::<&str>)?;
        let menu = Menu::with_items(app, &[&open, &compose, &separator, &quit])?;
        TrayIconBuilder::with_id("openpush")
            .icon(Image::from_bytes(include_bytes!("../icons/icon.png"))?)
            .tooltip("OpenPush")
            .menu(&menu)
            .show_menu_on_left_click(true)
            .on_menu_event(move |app, event| match event.id().as_ref() {
                "open" => show_main(app),
                "compose" => on_compose(app),
                "quit" => app.exit(0),
                _ => {}
            })
            .build(app)?;
        Ok(())
    };
    build().is_ok()
}

/// User-initiated only (tray/dock).
pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Validates a composer window label (`composer-<uuid>`).
pub fn composer_conversation(label: &str) -> Option<ConversationId> {
    label
        .strip_prefix(COMPOSER_PREFIX)
        .and_then(|id| id.parse().ok())
}

/// Opens (or raises, on explicit request) the frameless composer for one conversation. The
/// window shares the same native session/draft owner as the main window.
pub fn open_composer(app: &AppHandle, conversation: ConversationId) -> BridgeResult<()> {
    let label = format!("{COMPOSER_PREFIX}{conversation}");
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }
    let url = format!("index.html?window=composer&conversationId={conversation}");
    WebviewWindowBuilder::new(app, &label, WebviewUrl::App(url.into()))
        .title("OpenPush message")
        .inner_size(420.0, 560.0)
        .min_inner_size(320.0, 360.0)
        .decorations(false)
        .resizable(true)
        .build()
        .map(|_| ())
        .map_err(|_| BridgeError::new("window", "Could not open the composer window."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_labels_require_a_uuid() {
        let id = ConversationId::new();
        assert_eq!(composer_conversation(&format!("composer-{id}")), Some(id));
        assert_eq!(composer_conversation("composer-../../etc"), None);
        assert_eq!(composer_conversation("main"), None);
    }
}
