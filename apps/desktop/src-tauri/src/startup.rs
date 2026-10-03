//! Startup policy kept separate from OS registration side effects.
use tauri::Manager;
pub const BACKGROUND_ARGUMENT: &str = "--background";

pub fn background_requested(arguments: impl IntoIterator<Item = String>) -> bool {
    arguments
        .into_iter()
        .any(|argument| argument == BACKGROUND_ARGUMENT)
}

/// A failed tray must never create an unreachable hidden launch.
pub fn hide_initial_main(background: bool, tray_available: bool) -> bool {
    background && tray_available
}

/// Quiet login startup is allowed only after the tray is known reachable.
/// On macOS the app becomes an accessory while the main window is hidden;
/// `tray::show_main` restores regular activation policy.
pub fn background_main(app: &tauri::AppHandle) -> crate::error::BridgeResult<()> {
    let main = app
        .get_webview_window(crate::tray::MAIN)
        .ok_or_else(|| crate::error::BridgeError::new("window", "Main window is unavailable."))?;
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory)
        .map_err(|_| {
            crate::error::BridgeError::new("window", "Could not enter background mode.")
        })?;
    main.hide()
        .map_err(|_| crate::error::BridgeError::new("window", "Could not hide main window."))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_needs_explicit_argument_and_tray() {
        assert!(background_requested([BACKGROUND_ARGUMENT.into()]));
        assert!(!background_requested(["--other".into()]));
        assert!(hide_initial_main(true, true));
        assert!(!hide_initial_main(true, false));
    }
}
