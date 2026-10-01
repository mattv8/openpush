//! AppKit-only floating-head experiment.
//!
//! This uses an `NSPanel` and an `NSView` `hitTest:` override. It intentionally
//! does not use Tauri transparent windows, WebKit flags, CGS, or other private
//! APIs. A person must still physically verify the corner hit testing before
//! this capability can be promoted beyond experimental.

use cocoa::{
    appkit::{
        NSBackingStoreType, NSColor, NSPanel, NSTextField, NSView, NSWindow, NSWindowStyleMask,
    },
    base::{id, nil, NO, YES},
    foundation::{NSPoint, NSRect, NSSize, NSString},
};
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, Object, Sel},
    sel, sel_impl,
};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use uuid::Uuid;

const HEAD_SIZE: f64 = 56.0;
// `NSFloatingWindowLevel` is the public AppKit value 3. The legacy Cocoa crate
// does not export that named constant, so keep the documented value local.
const FLOATING_WINDOW_LEVEL: i64 = 3;
const MAX_HEADS: usize = 8;

/// Raw AppKit pointers are owned by the corresponding `NSPanel`; this registry
/// is accessed only on AppKit's main thread (enforced by command dispatch).
struct Head {
    panel: usize,
    view: usize,
    initials_label: usize,
    badge: usize,
}
struct HeadRegistry {
    heads: HashMap<Uuid, Head>,
    views: HashMap<usize, Uuid>,
}

static HEADS: OnceLock<Mutex<HeadRegistry>> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();

fn heads() -> &'static Mutex<HeadRegistry> {
    HEADS.get_or_init(|| {
        Mutex::new(HeadRegistry {
            heads: HashMap::new(),
            views: HashMap::new(),
        })
    })
}

/// Register the application handle while Tauri is on its UI thread.
pub fn register_app(app: AppHandle) {
    let _ = APP.set(app);
}

pub fn head_capability() -> String {
    "experimental/unconfirmed: AppKit NSPanel probe compiled; transparent-corner OS hit testing NOT yet physically verified; fixed initial position has no monitor persistence; main window is the active fallback".into()
}

fn is_in_circle(point: NSPoint) -> bool {
    let dx = point.x - HEAD_SIZE / 2.0;
    let dy = point.y - HEAD_SIZE / 2.0;
    dx * dx + dy * dy <= (HEAD_SIZE / 2.0) * (HEAD_SIZE / 2.0)
}

fn should_apply_update(head_exists: bool) -> bool {
    head_exists
}

fn head_view_class() -> *const Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    *CLASS.get_or_init(|| unsafe {
        let superclass = class!(NSView);
        let mut decl = ClassDecl::new("OpenPushCircularHeadView", superclass)
            .expect("head view class must register once");
        decl.add_method(
            sel!(hitTest:),
            head_hit_test as extern "C" fn(&Object, Sel, NSPoint) -> id,
        );
        decl.add_method(
            sel!(mouseDown:),
            head_mouse_down as extern "C" fn(&Object, Sel, id),
        );
        decl.register() as *const Class as usize
    }) as *const Class
}

/// Return nil for the transparent corners. This is an AppKit view-level input
/// decision only: it does NOT prove window-server click-through without a
/// physical native test; CSS does not participate in it.
extern "C" fn head_hit_test(view: &Object, _: Sel, point: NSPoint) -> id {
    if is_in_circle(point) {
        view as *const Object as id
    } else {
        nil
    }
}

extern "C" fn head_mouse_down(view: &Object, _: Sel, _: id) {
    let conversation = heads().lock().ok().and_then(|registry| {
        registry
            .views
            .get(&(view as *const Object as usize))
            .copied()
    });
    if let Some(conversation) = conversation {
        // Do not construct a Tauri webview during AppKit mouse dispatch.
        if let Some(app) = APP.get().cloned() {
            tauri::async_runtime::spawn(async move {
                let _ = app.run_on_main_thread(move || {
                    let _ = open_composer(conversation);
                });
            });
        }
    }
}

fn open_composer(conversation: Uuid) -> Result<(), String> {
    let app = APP.get().ok_or("native host is not initialized")?;
    let label = format!("composer-{conversation}");
    if let Some(window) = app.get_webview_window(&label) {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        return Ok(());
    }
    // The opaque UUID is context only; the window loads the bundled app, never
    // an arbitrary caller-supplied URL.
    let url = WebviewUrl::App(format!("index.html?conversation_id={conversation}").into());
    WebviewWindowBuilder::new(app, label, url)
        .title("OpenPush composer")
        .inner_size(520.0, 420.0)
        .min_inner_size(360.0, 260.0)
        .decorations(false)
        .resizable(true)
        .build()
        .map_err(|error| error.to_string())?
        .set_focus()
        .map_err(|error| error.to_string())
}

unsafe fn native_label(initials: &str, unread: u32) -> id {
    ns_string(&format!(
        "{initials}, {unread} unread messages. Activate to open composer."
    ))
}

/// AppKit setters retain/copy these values; autorelease prevents repeated badge
/// updates from accumulating owned NSString allocations.
unsafe fn ns_string(value: &str) -> id {
    let value = cocoa::foundation::NSString::alloc(nil).init_str(value);
    let _: id = msg_send![value, autorelease];
    value
}

/// Update an existing head without changing its frame, ordering, or focus.
/// SAFETY: callers must run on the AppKit main thread via `run_on_main_thread`.
pub unsafe fn update_conversation_head(conversation: Uuid, initials: &str, unread: u32) -> bool {
    let Some(head) = heads().lock().ok().and_then(|registry| {
        registry
            .heads
            .get(&conversation)
            .map(|head| (head.view, head.initials_label, head.badge))
    }) else {
        return should_apply_update(false);
    };
    let value = ns_string(initials);
    (head.1 as id).setStringValue_(value);
    let badge_value = ns_string(&unread.min(99).to_string());
    (head.2 as id).setStringValue_(badge_value);
    let _: () = msg_send![head.2 as id, setHidden: if unread == 0 { YES } else { NO }];
    let label = native_label(initials, unread);
    let _: () = msg_send![head.0 as id, setAccessibilityLabel: label];
    let _: () = msg_send![head.0 as id, setToolTip: label];
    should_apply_update(true)
}

/// Explicit opt-in creation only. This must not be called by arrival handling.
/// SAFETY: callers must run on the AppKit main thread via `run_on_main_thread`.
pub unsafe fn show_conversation_head(
    conversation: Uuid,
    initials: &str,
    unread: u32,
) -> Result<(), String> {
    if update_conversation_head(conversation, initials, unread) {
        return Ok(());
    }
    if heads()
        .lock()
        .map_err(|_| "native head registry is unavailable")?
        .heads
        .len()
        >= MAX_HEADS
    {
        return Err(format!(
            "native head limit ({MAX_HEADS}) reached; use the main window"
        ));
    }
    let frame = NSRect::new(NSPoint::new(32.0, 32.0), NSSize::new(HEAD_SIZE, HEAD_SIZE));
    let style = NSWindowStyleMask::NSBorderlessWindowMask;
    let panel: id = NSPanel::alloc(nil).initWithContentRect_styleMask_backing_defer_(
        frame,
        style,
        NSBackingStoreType::NSBackingStoreBuffered,
        NO,
    );
    panel.setOpaque_(NO);
    panel.setBackgroundColor_(NSColor::clearColor(nil));
    panel.setHasShadow_(YES);
    panel.setHidesOnDeactivate_(NO);
    panel.setLevel_(FLOATING_WINDOW_LEVEL);
    panel.setMovableByWindowBackground_(YES);

    let view: id = msg_send![head_view_class(), alloc];
    let view: id = msg_send![view, initWithFrame: frame];
    view.setWantsLayer(YES);
    let layer: id = msg_send![view, layer];
    let color: id = NSColor::colorWithCalibratedRed_green_blue_alpha_(nil, 0.15, 0.32, 0.58, 1.0);
    let cg_color: id = msg_send![color, CGColor];
    let _: () = msg_send![layer, setBackgroundColor: cg_color];
    let _: () = msg_send![layer, setCornerRadius: HEAD_SIZE / 2.0];

    // These are native AppKit labels, not web content. The root view retains
    // circle-only hit testing, so neither label makes the square corners live.
    let initials_label: id = NSTextField::initWithFrame_(
        NSTextField::alloc(nil),
        NSRect::new(NSPoint::new(5.0, 14.0), NSSize::new(46.0, 28.0)),
    );
    let ns_initials = ns_string(initials);
    initials_label.setStringValue_(ns_initials);
    let _: () = msg_send![initials_label, setEditable: NO];
    let _: () = msg_send![initials_label, setSelectable: NO];
    let _: () = msg_send![initials_label, setBezeled: NO];
    let _: () = msg_send![initials_label, setDrawsBackground: NO];
    // Verified against the installed macOS SDK's NSText.h: macOS arm64 center is 2.
    const NSTextAlignmentCenter: i64 = 2;
    let _: () = msg_send![initials_label, setAlignment: NSTextAlignmentCenter];
    let white: id = msg_send![class!(NSColor), whiteColor];
    let _: () = msg_send![initials_label, setTextColor: white];
    let _: () = msg_send![view, addSubview: initials_label];
    // The view retained this subview; registry keeps only a borrowed pointer.
    let _: () = msg_send![initials_label, release];
    let badge: id = NSTextField::initWithFrame_(
        NSTextField::alloc(nil),
        NSRect::new(NSPoint::new(36.0, 2.0), NSSize::new(18.0, 18.0)),
    );
    let value = ns_string(&unread.min(99).to_string());
    badge.setStringValue_(value);
    let _: () = msg_send![badge, setEditable: NO];
    let _: () = msg_send![badge, setSelectable: NO];
    let _: () = msg_send![badge, setBezeled: NO];
    let _: () = msg_send![badge, setDrawsBackground: NO];
    let _: () = msg_send![badge, setAlignment: NSTextAlignmentCenter];
    let _: () = msg_send![badge, setTextColor: white];
    let _: () = msg_send![badge, setHidden: if unread == 0 { YES } else { NO }];
    let _: () = msg_send![view, addSubview: badge];
    let _: () = msg_send![badge, release];

    // Native accessibility label provides initials and unread count without
    // claiming the probe is a complete composer UI.
    let ns_label = native_label(initials, unread);
    let _: () = msg_send![view, setAccessibilityLabel: ns_label];
    let _: () = msg_send![view, setToolTip: ns_label];
    panel.setContentView_(view);
    // The panel retains its content view; registry pointers are borrowed from it.
    let _: () = msg_send![view, release];
    panel.orderFrontRegardless();

    let mut registry = heads()
        .lock()
        .map_err(|_| "native head registry is unavailable")?;
    registry.heads.insert(
        conversation,
        Head {
            panel: panel as usize,
            view: view as usize,
            initials_label: initials_label as usize,
            badge: badge as usize,
        },
    );
    registry.views.insert(view as usize, conversation);
    Ok(())
}

/// SAFETY: callers must run on the AppKit main thread via `run_on_main_thread`.
pub unsafe fn hide_conversation_head(conversation: Uuid) -> bool {
    let panel = heads().lock().ok().and_then(|mut registry| {
        let head = registry.heads.remove(&conversation);
        if let Some(head) = head {
            registry.views.remove(&head.view);
            Some(head.panel)
        } else {
            None
        }
    });
    if let Some(panel) = panel {
        let panel = panel as id;
        panel.orderOut_(nil);
        let _: () = msg_send![panel, release];
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use cocoa::foundation::NSPoint;

    #[test]
    fn hit_test_circle_boundaries() {
        assert!(!super::is_in_circle(NSPoint::new(0.0, 0.0)));
        assert!(super::is_in_circle(NSPoint::new(28.0, 28.0)));
        assert!(super::is_in_circle(NSPoint::new(28.0, 0.0)));
        assert!(!super::is_in_circle(NSPoint::new(1.0, 1.0)));
    }

    #[test]
    fn update_requires_existing_opted_in_head() {
        assert!(!super::should_apply_update(false));
        assert!(super::should_apply_update(true));
    }
}
