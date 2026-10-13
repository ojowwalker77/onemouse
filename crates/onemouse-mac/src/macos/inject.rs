//! Posts the PC's input as CoreGraphics events, for when the PC is main.

use std::ptr;
use std::sync::Mutex;
use std::time::Instant;

use onemouse_protocol::{Message, MouseButton};

use super::ffi::*;
use crate::inject::{Backend, Injector, KeyEvent, Mouse};
use crate::server::Remote;

#[derive(Debug)]
struct Cg;

impl Cg {
    /// Posts and releases `event`; null (allocation failed) is skipped.
    fn post(event: CGEventRef, flags: u64) {
        if event.is_null() {
            return;
        }
        // SAFETY: a valid event we own; released after posting.
        unsafe {
            CGEventSetFlags(event, flags);
            CGEventPost(kCGHIDEventTap, event);
            CFRelease(event.cast_const());
        }
    }
}

impl Backend for Cg {
    fn mouse(
        &mut self,
        event: Mouse,
        (x, y): (f64, f64),
        (dx, dy): (f64, f64),
        clicks: i64,
        flags: u64,
    ) {
        let (ty, button) = match event {
            Mouse::Moved => (kCGEventMouseMoved, MouseButton::Left),
            Mouse::Dragged(b) => (
                match b {
                    MouseButton::Left => kCGEventLeftMouseDragged,
                    MouseButton::Right => kCGEventRightMouseDragged,
                    _ => kCGEventOtherMouseDragged,
                },
                b,
            ),
            Mouse::Down(b) => (
                match b {
                    MouseButton::Left => kCGEventLeftMouseDown,
                    MouseButton::Right => kCGEventRightMouseDown,
                    _ => kCGEventOtherMouseDown,
                },
                b,
            ),
            Mouse::Up(b) => (
                match b {
                    MouseButton::Left => kCGEventLeftMouseUp,
                    MouseButton::Right => kCGEventRightMouseUp,
                    _ => kCGEventOtherMouseUp,
                },
                b,
            ),
        };
        let number = match button {
            MouseButton::Left => kCGMouseButtonLeft,
            MouseButton::Right => kCGMouseButtonRight,
            MouseButton::Middle => kCGMouseButtonCenter,
            MouseButton::Back => 3,
            MouseButton::Forward => 4,
        };
        // SAFETY: plain constructor; the event is checked and released by `post`.
        unsafe {
            let event = CGEventCreateMouseEvent(ptr::null(), ty, CGPoint { x, y }, number);
            if event.is_null() {
                return;
            }
            CGEventSetIntegerValueField(event, kCGMouseEventButtonNumber, number.into());
            CGEventSetIntegerValueField(event, kCGMouseEventDeltaX, dx.round() as i64);
            CGEventSetIntegerValueField(event, kCGMouseEventDeltaY, dy.round() as i64);
            if clicks > 0 {
                CGEventSetIntegerValueField(event, kCGMouseEventClickState, clicks);
            }
            Self::post(event, flags);
        }
    }

    fn key(&mut self, vk: u16, event: KeyEvent, flags: u64) {
        // SAFETY: plain constructor; the event is checked and released by `post`.
        unsafe {
            let e = CGEventCreateKeyboardEvent(ptr::null(), vk, event == KeyEvent::Down);
            if event == KeyEvent::FlagsChanged && !e.is_null() {
                CGEventSetType(e, kCGEventFlagsChanged);
            }
            Self::post(e, flags);
        }
    }

    fn scroll(&mut self, dx: i32, dy: i32, flags: u64) {
        // Wheel 1 is vertical (positive = up), wheel 2 horizontal (positive
        // = left), the reverse of how the tap reads them.
        // SAFETY: plain constructor; the event is checked and released by `post`.
        let event = unsafe {
            CGEventCreateScrollWheelEvent2(ptr::null(), kCGScrollEventUnitPixel, 2, dy, -dx, 0)
        };
        Self::post(event, flags);
    }
}

/// The PC's input, applied on this Mac.
#[derive(Debug)]
pub struct MacRemote(Mutex<Injector<Cg>>);

impl MacRemote {
    pub fn new() -> Self {
        Self(Mutex::new(Injector::new(Cg, super::keyboard_is_iso())))
    }

    fn injector(&self) -> std::sync::MutexGuard<'_, Injector<Cg>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for MacRemote {
    fn default() -> Self {
        Self::new()
    }
}

impl Remote for MacRemote {
    fn input(&self, msg: &Message) {
        self.injector().handle(msg, Instant::now());
    }

    fn release(&self) {
        self.injector().release_all();
    }
}
