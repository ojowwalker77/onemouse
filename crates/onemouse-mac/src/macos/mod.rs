//! macOS glue: a session event tap feeds every input event to the
//! [`Controller`], swallows the ones that go to the secondary, and hides and
//! freezes the cursor while it's away.

// CoreGraphics names, matched on as constants.
#![allow(non_upper_case_globals)]

mod ffi;
mod ui;

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;

use onemouse_protocol::MouseButton;
use onemouse_protocol::key;

use self::ffi::*;
use crate::config::Config;
use crate::controller::{self, Controller, Input, Output};
use crate::keymap;
use crate::layout::{Point, Rect};
use crate::log;
use crate::pairing::Prompts;
use crate::server::Link;
use crate::server::Security;

/// Trackpad points → protocol scroll units (120 per notch).
const SCROLL_UNITS_PER_POINT: f64 = 2.0;
/// Mouse wheel lines → protocol scroll units (one notch is 3 lines).
const SCROLL_UNITS_PER_LINE: f64 = 40.0;

/// Asks for the permissions the event tap needs, opening the System Settings
/// prompts if missing. Returns whether everything is granted already.
pub fn ensure_permissions() -> bool {
    // SAFETY: plain CoreFoundation/AX calls with valid arguments; the
    // dictionary is released after use.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        );
        let accessibility = AXIsProcessTrustedWithOptions(options) != 0;
        CFRelease(options);
        let input_monitoring = CGPreflightListenEventAccess() || CGRequestListenEventAccess();
        accessibility && input_monitoring
    }
}

/// Whether both permissions are granted now, without prompting.
pub fn permissions_granted() -> bool {
    // SAFETY: no arguments.
    unsafe { AXIsProcessTrusted() != 0 && CGPreflightListenEventAccess() }
}

/// The Mac's displays in global points.
pub fn displays() -> Vec<Rect> {
    let mut ids = [0; 16];
    let mut count = 0;
    // SAFETY: the buffer holds `ids.len()` entries.
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    ids[..count as usize]
        .iter()
        // SAFETY: ids come from CGGetActiveDisplayList.
        .map(|&id| unsafe { CGDisplayBounds(id) })
        .map(|b| Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height))
        .collect()
}

/// Whether the built-in/attached keyboard has the ISO layout (§ swap).
pub fn keyboard_is_iso() -> bool {
    // SAFETY: no arguments beyond the value LMGetKbdType returns.
    unsafe { KBGetLayoutType(LMGetKbdType().into()) == kKeyboardISO }
}

/// What the app is started with.
pub struct App {
    pub link: Arc<Link>,
    pub security: Arc<Security>,
    /// Pairing questions from connection threads, answered in a dialog.
    pub prompts: Arc<Prompts>,
    pub config: Config,
    pub config_path: Option<PathBuf>,
    pub scroll_speed: f64,
    /// Open the Arrange Displays window at launch.
    pub arrange_at_start: bool,
}

/// Everything the event tap and the UI share. Only touched on the main
/// thread.
struct Tap {
    controller: RefCell<Controller>,
    link: Arc<Link>,
    security: Arc<Security>,
    prompts: Arc<Prompts>,
    config: RefCell<Config>,
    config_path: Option<PathBuf>,
    arrange_at_start: bool,
    iso: bool,
    scroll_speed: f64,
    port: Cell<CFMachPortRef>,
    cursor_hidden: Cell<bool>,
}

/// Installs the event tap and runs the menu-bar app until Quit.
pub fn run(controller: Controller, app: App) -> Result<(), String> {
    let tap: &'static Tap = Box::leak(Box::new(Tap {
        controller: RefCell::new(controller),
        link: app.link,
        security: app.security,
        prompts: app.prompts,
        config: RefCell::new(app.config),
        config_path: app.config_path,
        arrange_at_start: app.arrange_at_start,
        iso: keyboard_is_iso(),
        scroll_speed: app.scroll_speed,
        port: Cell::new(ptr::null_mut()),
        cursor_hidden: Cell::new(false),
    }));
    if tap.iso {
        log!("ISO keyboard detected");
    }

    let mut mask = 0u64;
    for ty in [
        kCGEventLeftMouseDown,
        kCGEventLeftMouseUp,
        kCGEventRightMouseDown,
        kCGEventRightMouseUp,
        kCGEventMouseMoved,
        kCGEventLeftMouseDragged,
        kCGEventRightMouseDragged,
        kCGEventKeyDown,
        kCGEventKeyUp,
        kCGEventFlagsChanged,
        kCGEventScrollWheel,
        kCGEventOtherMouseDown,
        kCGEventOtherMouseUp,
        kCGEventOtherMouseDragged,
    ]
    .into_iter()
    .chain(GESTURE_EVENT_TYPES)
    {
        mask |= 1 << ty;
    }

    // SAFETY: `tap` is 'static and only touched from this thread's run loop.
    unsafe {
        allow_hiding_cursor_in_background();
        let port = CGEventTapCreate(
            kCGSessionEventTap,
            kCGHeadInsertEventTap,
            kCGEventTapOptionDefault,
            mask,
            callback,
            ptr::from_ref(tap).cast_mut().cast(),
        );
        if port.is_null() {
            return Err(
                "couldn't create the event tap: grant Accessibility and Input \
                        Monitoring to this terminal in System Settings → Privacy & Security, \
                        then restart it"
                    .into(),
            );
        }
        tap.port.set(port);
        let source = CFMachPortCreateRunLoopSource(ptr::null(), port, 0);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(port, true);
    }
    ui::run(tap);
    Ok(())
}

unsafe extern "C" fn callback(
    _proxy: CGEventTapProxy,
    ty: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    // SAFETY: `user_info` is the leaked `Tap` passed in `run`.
    let tap = unsafe { &*user_info.cast::<Tap>() };
    if ty == kCGEventTapDisabledByTimeout || ty == kCGEventTapDisabledByUserInput {
        log!("event tap was disabled, re-enabling");
        // SAFETY: the port is valid for the life of the process.
        unsafe { CGEventTapEnable(tap.port.get(), true) };
        return event;
    }
    // Never unwind into CoreGraphics; on a bug, let the event through.
    let swallow = panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `event` is valid for the duration of the callback.
        let inputs = unsafe { translate(tap, ty, event) };
        let mut swallow = false;
        for input in inputs {
            swallow |= handle(tap, input);
        }
        swallow
    }))
    .unwrap_or(false);
    if swallow { ptr::null_mut() } else { event }
}

fn handle(tap: &Tap, input: Input) -> bool {
    let mac = if matches!(input, Input::Move { .. }) {
        displays()
    } else {
        Vec::new()
    };
    let out: Output = tap.link.with_peer(|peer| {
        let view = peer.map(|p| controller::Peer {
            id: p.id(),
            displays: &p.displays,
        });
        let mut out = tap.controller.borrow_mut().handle(input, &mac, view);
        if let Some(peer) = peer {
            if out.went_remote {
                // Until M2 anyone on the LAN can connect: make it visible.
                log!("cursor → {} ({})", peer.name, peer.addr);
            }
            for msg in out.send.drain(..) {
                peer.send(msg);
            }
        }
        out
    });
    if out.went_remote {
        set_cursor_away(tap, true);
    }
    if let Some(p) = out.went_local {
        // SAFETY: plain CoreGraphics calls.
        unsafe { CGWarpMouseCursorPosition(CGPoint { x: p.x, y: p.y }) };
        set_cursor_away(tap, false);
    }
    out.swallow
}

/// Hides and freezes the cursor (still receiving deltas) or brings it back.
fn set_cursor_away(tap: &Tap, away: bool) {
    // SAFETY: plain CoreGraphics calls; hide/show are kept balanced.
    unsafe {
        CGAssociateMouseAndMouseCursorPosition(u32::from(!away));
        if away != tap.cursor_hidden.get() {
            if away {
                CGDisplayHideCursor(CGMainDisplayID());
            } else {
                CGDisplayShowCursor(CGMainDisplayID());
            }
            tap.cursor_hidden.set(away);
        }
    }
}

/// Without this, `CGDisplayHideCursor` is ignored unless we're the frontmost
/// app, which a terminal-launched daemon never is.
unsafe fn allow_hiding_cursor_in_background() {
    // SAFETY: valid C string; the CFString is released after use.
    unsafe {
        let key = CFStringCreateWithCString(
            ptr::null(),
            c"SetsCursorInBackground".as_ptr(),
            kCFStringEncodingUTF8,
        );
        let cid = _CGSDefaultConnection();
        CGSSetConnectionProperty(cid, cid, key, kCFBooleanTrue);
        CFRelease(key);
    }
}

/// Turns one CGEvent into controller inputs (Caps Lock makes two).
unsafe fn translate(tap: &Tap, ty: u32, event: CGEventRef) -> Vec<Input> {
    // SAFETY (all fields below): the caller guarantees `event` is valid.
    let int = |field| unsafe { CGEventGetIntegerValueField(event, field) };
    let double = |field| unsafe { CGEventGetDoubleValueField(event, field) };
    let button = |pressed| {
        let button = match ty {
            kCGEventLeftMouseDown | kCGEventLeftMouseUp => MouseButton::Left,
            kCGEventRightMouseDown | kCGEventRightMouseUp => MouseButton::Right,
            _ => match int(kCGMouseEventButtonNumber) {
                2 => MouseButton::Middle,
                3 => MouseButton::Back,
                4 => MouseButton::Forward,
                _ => return vec![],
            },
        };
        vec![Input::Button { button, pressed }]
    };
    match ty {
        kCGEventMouseMoved
        | kCGEventLeftMouseDragged
        | kCGEventRightMouseDragged
        | kCGEventOtherMouseDragged => {
            // SAFETY: valid event.
            let pos = unsafe { CGEventGetLocation(event) };
            vec![Input::Move {
                pos: Point::new(pos.x, pos.y),
                dx: double(kCGMouseEventDeltaX),
                dy: double(kCGMouseEventDeltaY),
            }]
        }
        kCGEventLeftMouseDown | kCGEventRightMouseDown | kCGEventOtherMouseDown => button(true),
        kCGEventLeftMouseUp | kCGEventRightMouseUp | kCGEventOtherMouseUp => button(false),
        kCGEventScrollWheel => {
            // Positive Mac deltas reveal content above / to the left, the
            // protocol's dy > 0 is up and dx > 0 is right.
            let (x, y) = if int(kCGScrollWheelEventIsContinuous) != 0 {
                (
                    double(kCGScrollWheelEventPointDeltaAxis2) * SCROLL_UNITS_PER_POINT,
                    double(kCGScrollWheelEventPointDeltaAxis1) * SCROLL_UNITS_PER_POINT,
                )
            } else {
                (
                    double(kCGScrollWheelEventFixedPtDeltaAxis2) * SCROLL_UNITS_PER_LINE,
                    double(kCGScrollWheelEventFixedPtDeltaAxis1) * SCROLL_UNITS_PER_LINE,
                )
            };
            vec![Input::Scroll {
                dx: -x * tap.scroll_speed,
                dy: y * tap.scroll_speed,
            }]
        }
        kCGEventKeyDown | kCGEventKeyUp => {
            let vk = int(kCGKeyboardEventKeycode) as u16;
            keymap::from_mac(vk, tap.iso)
                .map(|code| Input::Key {
                    code,
                    pressed: ty == kCGEventKeyDown,
                })
                .into_iter()
                .collect()
        }
        kCGEventFlagsChanged => {
            let vk = int(kCGKeyboardEventKeycode) as u16;
            // SAFETY: valid event.
            let flags = unsafe { CGEventGetFlags(event) };
            modifier_change(vk, flags, tap.iso)
        }
        ty if GESTURE_EVENT_TYPES.contains(&ty) => vec![Input::Gesture],
        _ => vec![],
    }
}

/// A modifier key changed: work out which one and whether it's now down.
fn modifier_change(vk: u16, flags: u64, iso: bool) -> Vec<Input> {
    let Some(code) = keymap::from_mac(vk, iso) else {
        return vec![]; // Fn and friends
    };
    if code == key::CAPS_LOCK {
        // Reported once per toggle; Windows wants a full press.
        return vec![
            Input::Key {
                code,
                pressed: true,
            },
            Input::Key {
                code,
                pressed: false,
            },
        ];
    }
    let mask = match code {
        key::LEFT_CTRL => NX_DEVICELCTLKEYMASK,
        key::RIGHT_CTRL => NX_DEVICERCTLKEYMASK,
        key::LEFT_SHIFT => NX_DEVICELSHIFTKEYMASK,
        key::RIGHT_SHIFT => NX_DEVICERSHIFTKEYMASK,
        key::LEFT_ALT => NX_DEVICELALTKEYMASK,
        key::RIGHT_ALT => NX_DEVICERALTKEYMASK,
        key::LEFT_META => NX_DEVICELCMDKEYMASK,
        key::RIGHT_META => NX_DEVICERCMDKEYMASK,
        _ => return vec![],
    };
    vec![Input::Key {
        code,
        pressed: flags & mask != 0,
    }]
}
