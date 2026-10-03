//! Native Win32 conversation heads.  This module deliberately owns only the
//! shaped native windows; the host owns pins, composers, and generation fencing.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use tauri::AppHandle;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, CombineRgn, CreateEllipticRgn, CreateFontW, CreateRoundRectRgn,
            CreateSolidBrush, DrawTextW, EndPaint, EnumDisplayMonitors, FillRgn, GetMonitorInfoW,
            InvalidateRect, MonitorFromPoint, MonitorFromWindow, SelectObject, SetBkMode,
            SetTextColor, SetWindowRgn, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
            DEFAULT_PITCH, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD,
            MONITORINFOEXW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, OUT_DEFAULT_PRECIS,
            PAINTSTRUCT, RGN_OR, TRANSPARENT,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::{ReleaseCapture, SetCapture},
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, GetCursorPos, GetWindowRect, LoadCursorW, PostMessageW,
                RegisterClassW, SetForegroundWindow, SetWindowPos, SetWindowTextW, ShowWindow,
                TrackPopupMenu, HWND_TOPMOST, IDC_HAND, MF_STRING, SWP_NOACTIVATE, SWP_SHOWWINDOW,
                SW_SHOWNA, TPM_RETURNCMD, WM_CAPTURECHANGED, WM_CONTEXTMENU, WM_DESTROY,
                WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
                WM_NULL, WM_PAINT, WM_SETTINGCHANGE, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
                WS_EX_TOPMOST, WS_POPUP,
            },
        },
    },
};

use super::{HeadCallback, HeadEvent, HeadFrame, HeadPosition, HeadSpec};

const LOGICAL_SIZE: f64 = 56.0;
const CIRCLE_DIAMETER: f64 = 48.0;
const BADGE_HEIGHT: f64 = 18.0;
const BADGE_RING: f64 = 2.0;
const MAX_HEADS: usize = 8;
const DRAG_THRESHOLD: i32 = 4;
const DISMISS_MENU_ID: usize = 1;
const OPEN_MENU_ID: usize = 2;
const CLASS_NAME: &[u16] = &[
    b'O' as u16,
    b'p' as u16,
    b'e' as u16,
    b'n' as u16,
    b'P' as u16,
    b'u' as u16,
    b's' as u16,
    b'h' as u16,
    b'H' as u16,
    b'e' as u16,
    b'a' as u16,
    b'd' as u16,
    0,
];

#[derive(Clone)]
struct Head {
    conversation_id: String,
    generation: u64,
    initials: String,
    unread: u64,
    callback: HeadCallback,
    drag_origin: Option<POINT>,
    grab_offset: Option<(f64, f64)>,
    dragging: bool,
}

#[derive(Default)]
struct Registry {
    heads: HashMap<isize, Head>,
    class_registered: bool,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}
fn key(hwnd: HWND) -> isize {
    hwnd.0 as isize
}
fn hwnd(key: isize) -> HWND {
    HWND(key as _)
}

struct MonitorLookup<'a> {
    wanted: &'a str,
    found: Option<(windows::Win32::Graphics::Gdi::HMONITOR, MONITORINFOEXW)>,
}

unsafe extern "system" fn find_monitor_callback(
    monitor: windows::Win32::Graphics::Gdi::HMONITOR,
    _: windows::Win32::Graphics::Gdi::HDC,
    _: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    let lookup = &mut *(data.0 as *mut MonitorLookup<'_>);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool()
        && utf16_name(&info.szDevice) == lookup.wanted
    {
        lookup.found = Some((monitor, info));
        return false.into();
    }
    true.into()
}

fn with_ui_thread<T>(
    app: &AppHandle,
    action: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    // Tauri dispatches this closure to the thread that owns its Win32 event loop.
    // A synchronous channel keeps the public API's error contract without ever
    // holding the registry lock while entering Tauri or Win32.
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = send.send(action());
    })
    .map_err(|error| error.to_string())?;
    receive
        .recv()
        .map_err(|_| "Windows UI thread stopped".to_string())?
}

pub fn show_head(app: &AppHandle, spec: &HeadSpec, events: HeadCallback) -> Result<(), String> {
    let spec = spec.clone();
    with_ui_thread(app, move || unsafe { show_on_ui_thread(&spec, events) })
}

pub fn update_head(app: &AppHandle, spec: &HeadSpec) -> Result<(), String> {
    let spec = spec.clone();
    with_ui_thread(app, move || unsafe { update_on_ui_thread(&spec) })
}

pub fn hide_head(app: &AppHandle, conversation_id: &str) -> Result<(), String> {
    let conversation_id = conversation_id.to_owned();
    with_ui_thread(app, move || unsafe {
        let window = registry()
            .lock()
            .map_err(|_| "native head registry is unavailable")?
            .heads
            .iter()
            .find_map(|(hwnd, head)| (head.conversation_id == conversation_id).then_some(*hwnd));
        if let Some(window) = window {
            DestroyWindow(hwnd(window)).map_err(|error| error.to_string())?;
        }
        Ok(())
    })
}

pub fn clear_heads(app: &AppHandle) {
    let _ = with_ui_thread(app, || unsafe {
        let windows = registry()
            .lock()
            .map_err(|_| "native head registry is unavailable")?
            .heads
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for window in windows {
            let _ = DestroyWindow(hwnd(window));
        }
        Ok(())
    });
}

unsafe fn show_on_ui_thread(spec: &HeadSpec, callback: HeadCallback) -> Result<(), String> {
    let existing = {
        let state = registry()
            .lock()
            .map_err(|_| "native head registry is unavailable")?;
        state.heads.iter().find_map(|(hwnd, head)| {
            (head.conversation_id == spec.conversation_id).then_some(*hwnd)
        })
    };
    if let Some(window) = existing {
        update_existing(hwnd(window), spec, Some(callback))?;
        return Ok(());
    }
    {
        let state = registry()
            .lock()
            .map_err(|_| "native head registry is unavailable")?;
        if state.heads.len() >= MAX_HEADS {
            return Err("native head limit (8) reached".into());
        }
    }
    ensure_class()?;
    let (x, y, scale) = restore_position(&spec.position);
    let title = accessible_title(&spec.initials, spec.unread);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
        PCWSTR(CLASS_NAME.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_POPUP,
        x,
        y,
        (LOGICAL_SIZE * scale).round() as i32,
        (LOGICAL_SIZE * scale).round() as i32,
        None,
        None,
        Some(
            GetModuleHandleW(None)
                .map_err(|error| error.to_string())?
                .into(),
        ),
        None,
    )
    .map_err(|error| error.to_string())?;
    let head = Head {
        conversation_id: spec.conversation_id.clone(),
        generation: spec.generation,
        initials: spec.initials.clone(),
        unread: spec.unread,
        callback,
        drag_origin: None,
        grab_offset: None,
        dragging: false,
    };
    registry()
        .lock()
        .map_err(|_| "native head registry is unavailable")?
        .heads
        .insert(key(hwnd), head);
    let size = physical_size(hwnd);
    if !apply_shape(hwnd, size) {
        let _ = DestroyWindow(hwnd);
        return Err("Windows could not apply the circular input region".into());
    }
    let (x, y) = clamp_to_work_area(POINT { x, y }, size);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        x,
        y,
        size,
        size,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    let _ = ShowWindow(hwnd, SW_SHOWNA);
    emit_moved(hwnd, true);
    Ok(())
}

unsafe fn update_on_ui_thread(spec: &HeadSpec) -> Result<(), String> {
    let window = {
        let state = registry()
            .lock()
            .map_err(|_| "native head registry is unavailable")?;
        state.heads.iter().find_map(|(hwnd, head)| {
            (head.conversation_id == spec.conversation_id).then_some(*hwnd)
        })
    };
    let Some(window) = window else {
        return Err("native head does not exist".into());
    };
    update_existing(hwnd(window), spec, None)
}

unsafe fn update_existing(
    hwnd: HWND,
    spec: &HeadSpec,
    replacement: Option<HeadCallback>,
) -> Result<(), String> {
    let replacing_callback = replacement.is_some();
    let shape_changed = {
        let mut state = registry()
            .lock()
            .map_err(|_| "native head registry is unavailable")?;
        let head = state
            .heads
            .get_mut(&key(hwnd))
            .ok_or("native head does not exist")?;
        head.generation = spec.generation;
        head.initials = spec.initials.clone();
        let changed = badge_tier(head.unread) != badge_tier(spec.unread);
        head.unread = spec.unread;
        if let Some(callback) = replacement {
            head.callback = callback;
        }
        changed
    };
    if shape_changed && !apply_shape(hwnd, physical_size(hwnd)) {
        return Err("Windows could not update the head input region".into());
    }
    let _ = InvalidateRect(Some(hwnd), None, true);
    let title = accessible_title(&spec.initials, spec.unread);
    let _ = SetWindowTextW(hwnd, PCWSTR(title.as_ptr()));
    // Updates must never activate a head. Position is owned by native drag and
    // display reconciliation, so a stale async update cannot move it backwards.
    if replacing_callback {
        emit_moved(hwnd, false);
    }
    Ok(())
}

unsafe fn ensure_class() -> Result<(), String> {
    if registry()
        .lock()
        .map_err(|_| "native head registry is unavailable")?
        .class_registered
    {
        return Ok(());
    }
    let instance = GetModuleHandleW(None).map_err(|error| error.to_string())?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        hCursor: LoadCursorW(None, IDC_HAND).map_err(|error| error.to_string())?,
        lpszClassName: PCWSTR(CLASS_NAME.as_ptr()),
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        return Err("Windows could not register the head window class".into());
    }
    registry()
        .lock()
        .map_err(|_| "native head registry is unavailable")?
        .class_registered = true;
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_PAINT => {
            paint(hwnd);
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            if let Ok(mut state) = registry().lock() {
                if let Some(head) = state.heads.get_mut(&key(hwnd)) {
                    let mut rect = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rect);
                    let scale = GetDpiForWindow(hwnd) as f64 / 96.0;
                    head.drag_origin = Some(point);
                    head.grab_offset = Some((
                        (point.x - rect.left) as f64 / scale,
                        (point.y - rect.top) as f64 / scale,
                    ));
                    head.dragging = false;
                }
            }
            let _ = SetCapture(hwnd);
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            move_drag(hwnd, (_wparam.0 & 1) != 0);
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            finish_drag_or_activate(hwnd);
            return LRESULT(0);
        }
        WM_CONTEXTMENU => {
            show_context_menu(hwnd);
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            apply_dpi_change(hwnd, _lparam);
            return LRESULT(0);
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            reconcile(hwnd);
            return LRESULT(0);
        }
        WM_CAPTURECHANGED => {
            cancel_drag(hwnd);
            return LRESULT(0);
        }
        WM_DESTROY => {
            if let Ok(mut state) = registry().lock() {
                state.heads.remove(&key(hwnd));
            }
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, message, _wparam, _lparam)
}

unsafe fn move_drag(hwnd: HWND, left_button_down: bool) {
    if !left_button_down {
        cancel_drag(hwnd);
        return;
    }
    let mut cursor = POINT::default();
    if GetCursorPos(&mut cursor).is_err() {
        return;
    }
    let drag = registry().lock().ok().and_then(|mut state| {
        let head = state.heads.get_mut(&key(hwnd))?;
        let origin = head.drag_origin?;
        if drag_started(
            cursor.x - origin.x,
            cursor.y - origin.y,
            drag_threshold(hwnd),
        ) {
            head.dragging = true;
        }
        head.dragging.then_some(head.grab_offset?)
    });
    if let Some(grab_offset) = drag {
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let Some(info) = monitor_info(monitor) else {
            return;
        };
        let scale = monitor_scale(monitor);
        let (x, y, size) = drag_geometry(cursor, grab_offset, info.monitorInfo.rcWork, scale);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, size, size, SWP_NOACTIVATE);
        let _ = apply_shape(hwnd, size);
        emit_moved(hwnd, false);
    }
}

unsafe fn cancel_drag(hwnd: HWND) {
    let was_drag = registry().lock().ok().and_then(|mut state| {
        state.heads.get_mut(&key(hwnd)).map(|head| {
            let dragging = head.dragging;
            head.drag_origin = None;
            head.grab_offset = None;
            head.dragging = false;
            dragging
        })
    });
    if was_drag == Some(true) {
        emit_moved(hwnd, true);
    }
}

unsafe fn finish_drag_or_activate(hwnd: HWND) {
    let was_drag = registry()
        .lock()
        .ok()
        .and_then(|mut state| {
            state.heads.get_mut(&key(hwnd)).map(|head| {
                let dragging = head.dragging;
                head.drag_origin = None;
                head.grab_offset = None;
                head.dragging = false;
                dragging
            })
        })
        .unwrap_or(false);
    let _ = ReleaseCapture();
    if was_drag {
        emit_moved(hwnd, true);
    } else {
        emit(hwnd, EventKind::Activate);
    }
}

unsafe fn show_context_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let open: Vec<u16> = "Open conversation\0".encode_utf16().collect();
    let dismiss: Vec<u16> = "Dismiss floating conversation\0".encode_utf16().collect();
    let _ = AppendMenuW(menu, MF_STRING, OPEN_MENU_ID, PCWSTR(open.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, DISMISS_MENU_ID, PCWSTR(dismiss.as_ptr()));
    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(hwnd);
    let command = TrackPopupMenu(menu, TPM_RETURNCMD, point.x, point.y, None, hwnd, None);
    let _ = DestroyMenu(menu);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    match command.0 as usize {
        OPEN_MENU_ID => emit(hwnd, EventKind::Activate),
        DISMISS_MENU_ID => emit(hwnd, EventKind::Dismiss),
        _ => {}
    }
}

unsafe fn reconcile(hwnd: HWND) {
    let size = physical_size(hwnd);
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_ok() {
        let (x, y) = clamp_to_work_area(
            POINT {
                x: rect.left,
                y: rect.top,
            },
            size,
        );
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, size, size, SWP_NOACTIVATE);
        let _ = apply_shape(hwnd, size);
        emit_moved(hwnd, true);
    }
}

unsafe fn apply_dpi_change(hwnd: HWND, lparam: LPARAM) {
    // WM_DPICHANGED owns a RECT suggested by Windows. Copy it before any other
    // window call, then clamp it to the destination work area and rebuild HRGN.
    // Do not clamp or signal settled while a drag is active; emit unsettled for
    // current frame during active drag.
    let is_dragging = registry()
        .lock()
        .ok()
        .and_then(|state| state.heads.get(&key(hwnd)).map(|h| h.dragging))
        .unwrap_or(false);
    let suggested = (lparam.0 as *const RECT).as_ref().copied();
    let Some(suggested) = suggested else {
        return reconcile(hwnd);
    };
    let size = physical_size(hwnd);
    if is_dragging {
        let _ = apply_shape(hwnd, size);
        emit_moved(hwnd, false);
    } else {
        let (x, y) = clamp_to_work_area(
            POINT {
                x: suggested.left,
                y: suggested.top,
            },
            size,
        );
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, size, size, SWP_NOACTIVATE);
        let _ = apply_shape(hwnd, size);
        emit_moved(hwnd, true);
    }
}

unsafe fn apply_shape(hwnd: HWND, size: i32) -> bool {
    // SetWindowRgn takes ownership only on success; DeleteObject prevents a
    // failed shape update leaking the freshly allocated HRGN.
    let scale = size as f64 / LOGICAL_SIZE;
    let circle_size = scaled(CIRCLE_DIAMETER, scale);
    let region = CreateEllipticRgn(0, 0, circle_size, circle_size);
    let unread = registry()
        .lock()
        .ok()
        .and_then(|state| state.heads.get(&key(hwnd)).map(|head| head.unread))
        .unwrap_or_default();
    if unread > 0 {
        let badge_rect = badge_rect(unread, scale);
        let badge = CreateRoundRectRgn(
            badge_rect.left,
            badge_rect.top,
            badge_rect.right,
            badge_rect.bottom,
            scaled(BADGE_HEIGHT, scale),
            scaled(BADGE_HEIGHT, scale),
        );
        if CombineRgn(Some(region), Some(region), Some(badge), RGN_OR).0 == 0 {
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(badge.into());
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(region.into());
            return false;
        }
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(badge.into());
    }
    if SetWindowRgn(hwnd, Some(region), true) != 0 {
        true
    } else {
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(region.into());
        false
    }
}

fn scaled(logical: f64, scale: f64) -> i32 {
    (logical * scale).round().max(1.0) as i32
}

fn badge_tier(unread: u64) -> u8 {
    match unread {
        0 => 0,
        1..=9 => 1,
        10..=99 => 2,
        _ => 3,
    }
}

fn badge_width(unread: u64) -> f64 {
    match badge_tier(unread) {
        1 => 18.0,
        2 => 22.0,
        3 => 26.0,
        _ => 0.0,
    }
}

fn badge_rect(unread: u64, scale: f64) -> RECT {
    let right = scaled(LOGICAL_SIZE, scale);
    let bottom = right;
    let width = scaled(badge_width(unread), scale);
    let height = scaled(BADGE_HEIGHT, scale);
    RECT {
        left: right - width,
        top: bottom - height,
        right,
        bottom,
    }
}

fn badge_text(unread: u64) -> String {
    if unread >= 100 {
        "99+".to_string()
    } else {
        unread.to_string()
    }
}

unsafe fn clamp_to_work_area(point: POINT, size: i32) -> (i32, i32) {
    let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
    let Some(info) = monitor_info(monitor) else {
        return (point.x, point.y);
    };
    let work = info.monitorInfo.rcWork;
    (
        clamp_axis(point.x, work.left, work.right, size),
        clamp_axis(point.y, work.top, work.bottom, size),
    )
}

fn clamp_axis(value: i32, start: i32, end: i32, size: i32) -> i32 {
    value.clamp(start, (end - size).max(start))
}

fn drag_geometry(
    cursor: POINT,
    grab_offset: (f64, f64),
    work: RECT,
    scale: f64,
) -> (i32, i32, i32) {
    let size = (LOGICAL_SIZE * scale).round().max(1.0) as i32;
    let requested_x = (cursor.x as f64 - grab_offset.0 * scale).round() as i32;
    let requested_y = (cursor.y as f64 - grab_offset.1 * scale).round() as i32;
    (
        clamp_axis(requested_x, work.left, work.right, size),
        clamp_axis(requested_y, work.top, work.bottom, size),
        size,
    )
}

fn drag_started(dx: i32, dy: i32, threshold: i32) -> bool {
    dx.abs().max(dy.abs()) >= threshold
}

unsafe fn restore_position(position: &HeadPosition) -> (i32, i32, f64) {
    let fallback = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
    let selected = position
        .monitor
        .as_deref()
        .and_then(|name| find_named_monitor(name))
        .or_else(|| monitor_info(fallback).map(|info| (fallback, info)));
    let Some((monitor, info)) = selected else {
        return (0, 0, 1.0);
    };
    let scale = monitor_scale(monitor);
    let work = info.monitorInfo.rcWork;
    let x = work.left + (finite_or_zero(position.x) * scale).round() as i32;
    let y = work.top + (finite_or_zero(position.y) * scale).round() as i32;
    (x, y, scale)
}

unsafe fn find_named_monitor(
    name: &str,
) -> Option<(windows::Win32::Graphics::Gdi::HMONITOR, MONITORINFOEXW)> {
    let mut lookup = MonitorLookup {
        wanted: name,
        found: None,
    };
    let _ = EnumDisplayMonitors(
        None,
        None,
        Some(find_monitor_callback),
        LPARAM((&mut lookup as *mut MonitorLookup<'_>).cast::<std::ffi::c_void>() as isize),
    );
    lookup.found
}

unsafe fn monitor_info(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> Option<MONITORINFOEXW> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    GetMonitorInfoW(monitor, &mut info.monitorInfo)
        .as_bool()
        .then_some(info)
}

unsafe fn monitor_scale(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> f64 {
    let mut dpi_x = 96;
    let mut dpi_y = 96;
    let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
    dpi_x as f64 / 96.0
}

fn utf16_name(value: &[u16]) -> String {
    String::from_utf16_lossy(
        &value[..value
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(value.len())],
    )
}

unsafe fn physical_size(hwnd: HWND) -> i32 {
    ((LOGICAL_SIZE * GetDpiForWindow(hwnd) as f64 / 96.0).round() as i32).max(1)
}

unsafe fn drag_threshold(hwnd: HWND) -> i32 {
    ((DRAG_THRESHOLD as f64 * GetDpiForWindow(hwnd) as f64 / 96.0).round() as i32).max(1)
}

unsafe fn frame(hwnd: HWND) -> Option<(HeadPosition, HeadFrame)> {
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return None;
    }
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let Some(info) = monitor_info(monitor) else {
        return None;
    };
    let work = info.monitorInfo.rcWork;
    let scale = monitor_scale(monitor);
    Some((
        HeadPosition {
            monitor: Some(utf16_name(&info.szDevice)),
            x: (rect.left - work.left) as f64 / scale,
            y: (rect.top - work.top) as f64 / scale,
        },
        HeadFrame {
            x: rect.left as f64,
            y: rect.top as f64,
            width: (rect.right - rect.left) as f64,
            height: (rect.bottom - rect.top) as f64,
            work_x: work.left as f64,
            work_y: work.top as f64,
            work_width: (work.right - work.left) as f64,
            work_height: (work.bottom - work.top) as f64,
            scale_factor: scale,
        },
    ))
}

enum EventKind {
    Activate,
    Dismiss,
}
unsafe fn emit(hwnd: HWND, kind: EventKind) {
    let entry = registry().lock().ok().and_then(|state| {
        state.heads.get(&key(hwnd)).map(|head| {
            (
                head.callback.clone(),
                head.conversation_id.clone(),
                head.generation,
            )
        })
    });
    if let Some((callback, conversation_id, generation)) = entry {
        callback(match kind {
            EventKind::Activate => HeadEvent::Activate {
                conversation_id,
                generation,
            },
            EventKind::Dismiss => HeadEvent::Dismiss {
                conversation_id,
                generation,
            },
        });
    }
}
unsafe fn emit_moved(hwnd: HWND, settled: bool) {
    let entry = registry().lock().ok().and_then(|state| {
        state.heads.get(&key(hwnd)).map(|head| {
            (
                head.callback.clone(),
                head.conversation_id.clone(),
                head.generation,
            )
        })
    });
    if let (Some((callback, conversation_id, generation)), Some((position, frame))) =
        (entry, frame(hwnd))
    {
        callback(HeadEvent::Moved {
            conversation_id,
            generation,
            position,
            frame,
            settled,
        });
    }
}

unsafe fn create_scaled_font(
    logical_height: f64,
    scale: f64,
) -> windows::Win32::Graphics::Gdi::HFONT {
    create_scaled_font_for_face(logical_height, scale, "Segoe UI")
}

unsafe fn create_scaled_font_for_face(
    logical_height: f64,
    scale: f64,
    face_name: &str,
) -> windows::Win32::Graphics::Gdi::HFONT {
    let face: Vec<u16> = format!("{face_name}\0").encode_utf16().collect();
    CreateFontW(
        -(logical_height * scale).round() as i32,
        0,
        0,
        0,
        FW_BOLD.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
        PCWSTR(face.as_ptr()),
    )
}

unsafe fn draw_centered_text(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    value: &str,
    rect: &mut RECT,
    logical_height: f64,
    scale: f64,
) {
    let font = create_scaled_font(logical_height, scale);
    let old = (!font.is_invalid()).then(|| SelectObject(hdc, font.into()));
    let mut text: Vec<u16> = value.encode_utf16().collect();
    let _ = DrawTextW(hdc, &mut text, rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    if let Some(old) = old {
        let _ = SelectObject(hdc, old);
    }
    if !font.is_invalid() {
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(font.into());
    }
}

unsafe fn draw_person_icon(hdc: windows::Win32::Graphics::Gdi::HDC, scale: f64) {
    let icon_size = scaled(22.0, scale);
    let circle_size = scaled(CIRCLE_DIAMETER, scale);
    let mut rect = RECT {
        left: (circle_size - icon_size) / 2,
        top: (circle_size - icon_size) / 2,
        right: (circle_size + icon_size) / 2,
        bottom: (circle_size + icon_size) / 2,
    };
    let glyph = "\u{e77b}";
    let mdl2_font = create_scaled_font_for_face(22.0, scale, "Segoe MDL2 Assets");
    if !mdl2_font.is_invalid() {
        let old = SelectObject(hdc, mdl2_font.into());
        let mut glyph_units: Vec<u16> = glyph.encode_utf16().collect();
        let _ = DrawTextW(
            hdc,
            &mut glyph_units,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        let _ = SelectObject(hdc, old);
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(mdl2_font.into());
        return;
    }

    let fluent_font = create_scaled_font_for_face(22.0, scale, "Segoe Fluent Icons");
    if !fluent_font.is_invalid() {
        let old = SelectObject(hdc, fluent_font.into());
        let mut glyph_units: Vec<u16> = glyph.encode_utf16().collect();
        let _ = DrawTextW(
            hdc,
            &mut glyph_units,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        let _ = SelectObject(hdc, old);
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(fluent_font.into());
        return;
    }

    let center = scaled(CIRCLE_DIAMETER / 2.0, scale);
    let head_radius = scaled(6.0, scale);
    let head_center_y = scaled(18.0, scale);
    let shoulder_half_width = scaled(10.0, scale);
    let shoulder_half_height = scaled(7.0, scale);
    let shoulder_center_y = scaled(31.0, scale);
    let white = CreateSolidBrush(COLORREF(0xFFFFFF));
    let head = CreateEllipticRgn(
        center - head_radius,
        head_center_y - head_radius,
        center + head_radius,
        head_center_y + head_radius,
    );
    let shoulders = CreateEllipticRgn(
        center - shoulder_half_width,
        shoulder_center_y - shoulder_half_height,
        center + shoulder_half_width,
        shoulder_center_y + shoulder_half_height,
    );
    let _ = FillRgn(hdc, head, white);
    let _ = FillRgn(hdc, shoulders, white);
    let _ = windows::Win32::Graphics::Gdi::DeleteObject(head.into());
    let _ = windows::Win32::Graphics::Gdi::DeleteObject(shoulders.into());
    let _ = windows::Win32::Graphics::Gdi::DeleteObject(white.into());
}

unsafe fn paint(hwnd: HWND) {
    let Some((initials, unread)) = registry().lock().ok().and_then(|state| {
        state
            .heads
            .get(&key(hwnd))
            .map(|head| (head.initials.clone(), head.unread))
    }) else {
        return;
    };
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let dpi_scale = GetDpiForWindow(hwnd) as f64 / 96.0;
    let circle_size = scaled(CIRCLE_DIAMETER, dpi_scale);
    let circle = CreateEllipticRgn(0, 0, circle_size, circle_size);
    let blue = CreateSolidBrush(COLORREF(0xA34A24));
    let _ = FillRgn(hdc, circle, blue);
    let _ = windows::Win32::Graphics::Gdi::DeleteObject(circle.into());
    let _ = windows::Win32::Graphics::Gdi::DeleteObject(blue.into());
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = SetTextColor(hdc, COLORREF(0xFFFFFF));
    if initials.is_empty() {
        draw_person_icon(hdc, dpi_scale);
    } else {
        let inset = scaled(4.0, dpi_scale);
        let mut text = RECT {
            left: inset,
            top: inset,
            right: circle_size - inset,
            bottom: circle_size - inset,
        };
        draw_centered_text(
            hdc,
            &initials.chars().take(2).collect::<String>(),
            &mut text,
            18.0,
            dpi_scale,
        );
    }
    if unread > 0 {
        let mut badge_rect = badge_rect(unread, dpi_scale);
        let badge = CreateRoundRectRgn(
            badge_rect.left,
            badge_rect.top,
            badge_rect.right,
            badge_rect.bottom,
            scaled(BADGE_HEIGHT, dpi_scale),
            scaled(BADGE_HEIGHT, dpi_scale),
        );
        let blue = CreateSolidBrush(COLORREF(0xA34A24));
        let _ = FillRgn(hdc, badge, blue);
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(badge.into());
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(blue.into());
        let ring = scaled(BADGE_RING, dpi_scale);
        let inner = CreateRoundRectRgn(
            badge_rect.left + ring,
            badge_rect.top + ring,
            badge_rect.right - ring,
            badge_rect.bottom - ring,
            scaled(BADGE_HEIGHT - BADGE_RING * 2.0, dpi_scale),
            scaled(BADGE_HEIGHT - BADGE_RING * 2.0, dpi_scale),
        );
        let red = CreateSolidBrush(COLORREF(0x2835D9));
        let _ = FillRgn(hdc, inner, red);
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(inner.into());
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(red.into());
        let mut inner_rect = RECT {
            left: badge_rect.left + ring,
            top: badge_rect.top + ring,
            right: badge_rect.right - ring,
            bottom: badge_rect.bottom - ring,
        };
        draw_centered_text(hdc, &badge_text(unread), &mut inner_rect, 10.0, dpi_scale);
    }
    let _ = EndPaint(hwnd, &ps);
}

fn finite_or_zero(value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}
fn accessible_title(initials: &str, unread: u64) -> Vec<u16> {
    let contact = if initials.is_empty() {
        "Unknown contact"
    } else {
        initials
    };
    format!("Floating conversation {contact}, {unread} unread messages")
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn clamps_heads_inside_normal_and_tiny_work_areas() {
        assert_eq!(super::clamp_axis(-20, 10, 210, 56), 10);
        assert_eq!(super::clamp_axis(500, 10, 210, 56), 154);
        assert_eq!(super::clamp_axis(500, 10, 30, 56), 10);
    }

    #[test]
    fn drag_threshold_distinguishes_click_from_drag() {
        assert!(!super::drag_started(3, 0, 4));
        assert!(super::drag_started(4, 0, 4));
        assert!(super::drag_started(0, -4, 4));
    }

    #[test]
    fn badge_tiers_and_rects_follow_the_fixed_head_canvas() {
        assert_eq!(super::badge_tier(0), 0);
        assert_eq!(super::badge_tier(9), 1);
        assert_eq!(super::badge_tier(10), 2);
        assert_eq!(super::badge_tier(100), 3);
        let rect = |unread| {
            let rect = super::badge_rect(unread, 1.0);
            (rect.left, rect.top, rect.right, rect.bottom)
        };
        assert_eq!(rect(1), (38, 38, 56, 56));
        assert_eq!(rect(10), (34, 38, 56, 56));
        assert_eq!(rect(100), (30, 38, 56, 56));
        assert_eq!(super::badge_text(100), "99+");
    }

    #[test]
    fn empty_initials_use_unknown_contact_in_accessibility_title() {
        let title = String::from_utf16_lossy(&super::accessible_title("", 3));
        assert_eq!(
            title.trim_end_matches('\0'),
            "Floating conversation Unknown contact, 3 unread messages"
        );
    }

    #[test]
    fn cross_monitor_drag_uses_cursor_monitor_scale_and_negative_origin() {
        let (x, y, size) = super::drag_geometry(
            windows::Win32::Foundation::POINT { x: -900, y: 300 },
            (20.0, 12.0),
            windows::Win32::Foundation::RECT {
                left: -1600,
                top: -200,
                right: 0,
                bottom: 700,
            },
            1.25,
        );
        assert_eq!((x, y, size), (-925, 285, 70));
    }

    #[test]
    fn cross_monitor_drag_clamps_with_destination_dpi_size() {
        let (x, y, size) = super::drag_geometry(
            windows::Win32::Foundation::POINT { x: 2200, y: 500 },
            (28.0, 28.0),
            windows::Win32::Foundation::RECT {
                left: 1440,
                top: 0,
                right: 2040,
                bottom: 400,
            },
            2.0,
        );
        assert_eq!((x, y, size), (1928, 288, 112));
    }
}
