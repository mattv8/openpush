//! Tray/background behavior and the single conversation-composer window factory.
use crate::error::{BridgeError, BridgeResult};
use openpush_client_core::ConversationId;
use tauri::{
    image::Image,
    menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::TrayIconBuilder,
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};

pub const MAIN: &str = "main";
pub const COMPOSER_PREFIX: &str = "composer-";
const TRAY_ID: &str = "openpush";
const HEAD_OPEN_PREFIX: &str = "head-open:";
const HEAD_DISMISS_PREFIX: &str = "head-dismiss:";
const PANEL_WIDTH: f64 = 340.0;
const PANEL_HEIGHT: f64 = 440.0;
const PANEL_GAP: f64 = 8.0;

#[cfg(any(target_os = "macos", test))]
fn template_icon_rgba(size: u32) -> Vec<u8> {
    let mut rgba = vec![0; (size * size * 4) as usize];
    let center = (size as i32 - 1) / 2;
    let radius = (size as i32 / 2).saturating_sub(2);
    for y in 0..size as i32 {
        for x in 0..size as i32 {
            let alpha = if (x - center).abs() + (y - center).abs() <= radius {
                255
            } else {
                0
            };
            let offset = ((y as u32 * size + x as u32) * 4) as usize;
            rgba[offset..offset + 4].copy_from_slice(&[0, 0, 0, alpha]);
        }
    }
    rgba
}

fn tray_icon() -> tauri::Result<Image<'static>> {
    #[cfg(target_os = "macos")]
    {
        const SIZE: u32 = 18;
        Ok(Image::new_owned(template_icon_rgba(SIZE), SIZE, SIZE))
    }
    #[cfg(not(target_os = "macos"))]
    Image::from_bytes(include_bytes!("../icons/icon.png"))
}

/// Builds the tray; returns false when the platform/session offers no tray.
pub fn install(
    app: &AppHandle,
    on_compose: impl Fn(&AppHandle) + Send + Sync + 'static,
    on_quit: impl Fn(&AppHandle) + Send + Sync + 'static,
) -> bool {
    let build = || -> tauri::Result<()> {
        let menu = build_menu(app, &[])?;
        TrayIconBuilder::with_id(TRAY_ID)
            .icon(tray_icon()?)
            .icon_as_template(cfg!(target_os = "macos"))
            .tooltip("OpenPush")
            .menu(&menu)
            .show_menu_on_left_click(true)
            .on_menu_event(move |app, event| {
                let id = event.id().as_ref();
                if id == "open" {
                    show_main(app);
                } else if id == "compose" {
                    on_compose(app);
                } else if id == "quit" {
                    on_quit(app);
                } else if let Some(conversation_id) = id.strip_prefix(HEAD_OPEN_PREFIX) {
                    crate::heads_runtime::activate_from_tray(app, conversation_id);
                } else if let Some(conversation_id) = id.strip_prefix(HEAD_DISMISS_PREFIX) {
                    crate::heads_runtime::dismiss_from_tray(app, conversation_id);
                }
            })
            .build(app)?;
        Ok(())
    };
    build().is_ok()
}

fn build_menu(app: &AppHandle, heads: &[(String, String)]) -> tauri::Result<Menu<tauri::Wry>> {
    let open = MenuItem::with_id(app, "open", "Open OpenPush", true, None::<&str>)?;
    let compose = MenuItem::with_id(app, "compose", "New message", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit OpenPush", true, None::<&str>)?;

    let mut head_items = Vec::with_capacity(heads.len() * 2);
    for (conversation_id, name) in heads {
        let name = menu_name(name);
        head_items.push(MenuItem::with_id(
            app,
            format!("{HEAD_OPEN_PREFIX}{conversation_id}"),
            format!("Open {name}"),
            true,
            None::<&str>,
        )?);
        head_items.push(MenuItem::with_id(
            app,
            format!("{HEAD_DISMISS_PREFIX}{conversation_id}"),
            format!("Dismiss {name}"),
            true,
            None::<&str>,
        )?);
    }
    let head_refs = head_items
        .iter()
        .map(|item| item as &dyn IsMenuItem<tauri::Wry>)
        .collect::<Vec<_>>();
    let floating = Submenu::with_id_and_items(
        app,
        "floating-conversations",
        "Floating conversations",
        !heads.is_empty(),
        &head_refs,
    )?;
    Menu::with_items(app, &[&open, &compose, &floating, &separator, &quit])
}

fn menu_name(name: &str) -> String {
    let cleaned = name
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        "conversation".into()
    } else {
        cleaned.chars().take(60).collect()
    }
}

/// Rebuilds only the tray menu. Names are resolved from the current unlocked
/// session and are never persisted in the pin file.
pub fn set_floating_conversations(app: &AppHandle, heads: &[(String, String)]) -> bool {
    let Ok(menu) = build_menu(app, heads) else {
        return false;
    };
    app.tray_by_id(TRAY_ID)
        .is_some_and(|tray| tray.set_menu(Some(menu)).is_ok())
}

/// User-initiated only (tray/dock).
pub fn show_main(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    }
    if let Some(window) = app.get_webview_window(MAIN) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub fn composer_conversation(label: &str) -> Option<ConversationId> {
    label
        .strip_prefix(COMPOSER_PREFIX)
        .and_then(|id| id.parse().ok())
}

pub fn composer_url(conversation: ConversationId, head_panel: bool) -> String {
    let head = if head_panel { "&head=1" } else { "" };
    format!("index.html?window=composer&conversationId={conversation}{head}")
}

/// Opens or converts the one composer window for a conversation. This is an
/// explicit action and therefore focuses the resulting editable window.
pub fn open_composer_mode(
    app: &AppHandle,
    conversation: ConversationId,
    head_panel: bool,
) -> BridgeResult<()> {
    let effective_panel =
        head_panel || crate::heads_runtime::conversation_is_panel(app, &conversation.to_string());
    let label = format!("{COMPOSER_PREFIX}{conversation}");
    if let Some(window) = app.get_webview_window(&label) {
        configure_composer(&window, effective_panel);
        window
            .show()
            .map_err(|_| BridgeError::new("window", "Could not show the composer window."))?;
        window
            .set_focus()
            .map_err(|_| BridgeError::new("window", "Could not focus the composer window."))?;
        return Ok(());
    }
    let url = composer_url(conversation, effective_panel);
    let (width, height) = composer_size(effective_panel);
    let builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::App(url.into()))
        .title("OpenPush message")
        .inner_size(width, height)
        .min_inner_size(320.0, 360.0)
        .decorations(false)
        .resizable(true)
        .focused(true);
    let builder = if effective_panel {
        builder.always_on_top(true)
    } else {
        builder
    };
    builder
        .build()
        .map(|_| ())
        .map_err(|_| BridgeError::new("window", "Could not open the composer window."))
}

pub fn open_composer(app: &AppHandle, conversation: ConversationId) -> BridgeResult<()> {
    open_composer_mode(app, conversation, false)
}

pub fn open_head_panel(app: &AppHandle, conversation: ConversationId) -> BridgeResult<()> {
    open_composer_mode(app, conversation, true)
}

pub fn set_head_panel(app: &AppHandle, conversation_id: &str, panel: bool) {
    let label = format!("{COMPOSER_PREFIX}{conversation_id}");
    if let Some(window) = app.get_webview_window(&label) {
        configure_composer(&window, panel);
    }
}

fn composer_size(head_panel: bool) -> (f64, f64) {
    if head_panel {
        (PANEL_WIDTH, PANEL_HEIGHT)
    } else {
        (420.0, 560.0)
    }
}

fn configure_composer(window: &tauri::WebviewWindow, head_panel: bool) {
    let (width, height) = composer_size(head_panel);
    let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize::new(width, height)));
    let _ = window.set_always_on_top(head_panel);
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PanelRect {
    x: f64,
    y: f64,
}

fn panel_rect(frame: &crate::windows::HeadFrame, logical: bool) -> PanelRect {
    let scale = if frame.scale_factor.is_finite() {
        frame.scale_factor.max(1.0)
    } else {
        1.0
    };
    let unit = if logical { scale } else { 1.0 };
    let x0 = frame.x / unit;
    let y0 = frame.y / unit;
    let head_height = frame.height / unit;
    let work_x = frame.work_x / unit;
    let work_y = frame.work_y / unit;
    let work_width = frame.work_width / unit;
    let work_height = frame.work_height / unit;
    let size_scale = if logical { 1.0 } else { scale };
    let width = PANEL_WIDTH * size_scale;
    let height = PANEL_HEIGHT * size_scale;
    let gap = PANEL_GAP * size_scale;
    let max_x = (work_x + work_width - width).max(work_x);
    let x = x0.clamp(work_x, max_x);
    let below = y0 + head_height + gap;
    let above = y0 - gap - height;
    let max_y = (work_y + work_height - height).max(work_y);
    let y = if below <= max_y {
        below
    } else if above >= work_y {
        above
    } else {
        y0.clamp(work_y, max_y)
    };
    PanelRect { x, y }
}

/// Follows the backend-authoritative physical frame without changing logical
/// panel mode or size on every pointer movement.
pub fn place_head_panel(app: &AppHandle, conversation_id: &str, frame: &crate::windows::HeadFrame) {
    let label = format!("{COMPOSER_PREFIX}{conversation_id}");
    let Some(window) = app.get_webview_window(&label) else {
        return;
    };
    if !crate::heads_runtime::is_panel(app, &label) {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let rect = panel_rect(frame, true);
        let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition::new(
            rect.x, rect.y,
        )));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let rect = panel_rect(frame, false);
        let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
            rect.x.round() as i32,
            rect.y.round() as i32,
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::HeadFrame;

    fn frame(x: f64, y: f64, work_width: f64, work_height: f64, scale: f64) -> HeadFrame {
        HeadFrame {
            x,
            y,
            width: 56.0 * scale,
            height: 56.0 * scale,
            work_x: 0.0,
            work_y: 0.0,
            work_width,
            work_height,
            scale_factor: scale,
        }
    }

    #[test]
    fn composer_labels_require_a_uuid() {
        let id = ConversationId::new();
        assert_eq!(composer_conversation(&format!("composer-{id}")), Some(id));
        assert_eq!(composer_conversation("composer-../../etc"), None);
        assert_eq!(composer_conversation("main"), None);
    }

    #[test]
    fn panel_anchors_below_when_it_fits() {
        assert_eq!(
            panel_rect(&frame(20.0, 10.0, 1000.0, 800.0, 1.0), false),
            PanelRect { x: 20.0, y: 74.0 }
        );
    }

    #[test]
    fn panel_anchors_above_near_bottom_edge() {
        assert_eq!(
            panel_rect(&frame(20.0, 700.0, 1000.0, 800.0, 1.0), false),
            PanelRect { x: 20.0, y: 252.0 }
        );
    }

    #[test]
    fn panel_clamps_at_right_edge() {
        assert_eq!(
            panel_rect(&frame(980.0, 10.0, 1000.0, 800.0, 1.0), false).x,
            660.0
        );
    }

    #[test]
    fn panel_uses_work_area_origin_when_area_is_tiny() {
        assert_eq!(
            panel_rect(&frame(80.0, 80.0, 200.0, 200.0, 1.0), false),
            PanelRect { x: 0.0, y: 0.0 }
        );
    }

    #[test]
    fn panel_geometry_scales_in_physical_pixels() {
        assert_eq!(
            panel_rect(&frame(1900.0, 10.0, 2000.0, 1600.0, 2.0), false),
            PanelRect {
                x: 1320.0,
                y: 138.0
            }
        );
    }

    #[test]
    fn macos_logical_panel_coordinates_divide_global_physical_frame_by_scale() {
        assert_eq!(
            panel_rect(&frame(1900.0, 20.0, 2000.0, 1600.0, 2.0), true),
            PanelRect { x: 660.0, y: 74.0 }
        );
    }

    #[test]
    fn template_icon_has_transparent_corners_and_visible_diamond() {
        let size = 18;
        let rgba = template_icon_rgba(size);
        assert_eq!(rgba[3], 0);
        assert_eq!(rgba[(((size / 2) * size + size / 2) * 4 + 3) as usize], 255);
        assert!(rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 0));
        assert!(rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 255));
    }

    #[test]
    fn tray_names_strip_controls_and_are_bounded() {
        assert_eq!(menu_name(" Alice\n  Example "), "Alice Example");
        assert_eq!(menu_name("\n\t"), "conversation");
        assert_eq!(menu_name(&"x".repeat(80)).chars().count(), 60);
    }
}
