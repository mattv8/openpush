//! Native desktop window probes. Platform modules are deliberately small: the
//! webview never decides native input regions or focus behavior.

#[cfg(all(feature = "native-head-probe", target_os = "macos"))]
pub mod macos;

#[cfg(all(feature = "native-head-probe", target_os = "macos"))]
pub use macos::{
    head_capability, hide_conversation_head, show_conversation_head, update_conversation_head,
};

#[cfg(not(feature = "native-head-probe"))]
pub fn head_capability() -> String {
    "build-disabled: experimental native head probe was not compiled; main window is the active fallback".into()
}

#[cfg(all(feature = "native-head-probe", not(target_os = "macos")))]
pub fn head_capability() -> String {
    "experimental/unconfirmed: no public native head implementation has been compiled for this platform; main window is the active fallback".into()
}
