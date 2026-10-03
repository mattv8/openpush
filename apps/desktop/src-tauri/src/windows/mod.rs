//! Target-gated native floating conversation heads.
//!
//! The host owns persistence and generation fencing; these backends own only
//! native UI and must invoke callbacks without holding backend registry locks.
use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadPosition {
    pub monitor: Option<String>,
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug)]
pub struct HeadSpec {
    pub conversation_id: String,
    pub generation: u64,
    pub initials: String,
    pub unread: u64,
    pub position: HeadPosition,
}

/// Global physical desktop coordinates, using a top-left origin on all targets.
#[derive(Clone, Debug)]
pub struct HeadFrame {
    pub x: f64,
    pub y: f64,
    /// Contractual data used on Windows; unused on macOS (only in tests).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub width: f64,
    pub height: f64,
    pub work_x: f64,
    pub work_y: f64,
    pub work_width: f64,
    pub work_height: f64,
    pub scale_factor: f64,
}

#[derive(Clone, Debug)]
pub enum HeadEvent {
    Activate {
        conversation_id: String,
        generation: u64,
    },
    Dismiss {
        conversation_id: String,
        generation: u64,
    },
    Moved {
        conversation_id: String,
        generation: u64,
        position: HeadPosition,
        frame: HeadFrame,
        settled: bool,
    },
}

pub type HeadCallback = Arc<dyn Fn(HeadEvent) + Send + Sync>;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub use macos::{clear_heads, hide_head, show_head, update_head};
#[cfg(target_os = "windows")]
pub use windows::{clear_heads, hide_head, show_head, update_head};

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn show_head(_: &tauri::AppHandle, _: &HeadSpec, _: HeadCallback) -> Result<(), String> {
    Err("Floating conversation heads are unavailable on this platform.".into())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn update_head(_: &tauri::AppHandle, _: &HeadSpec) -> Result<(), String> {
    Err("Floating conversation heads are unavailable on this platform.".into())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn hide_head(_: &tauri::AppHandle, _: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn clear_heads(_: &tauri::AppHandle) {}
