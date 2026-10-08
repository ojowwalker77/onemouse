//! The handful of CoreGraphics / CoreFoundation / Carbon calls we need.

#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::{c_char, c_void};

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFMachPortRef = *mut c_void;
pub type CFRunLoopRef = *mut c_void;
pub type CFRunLoopSourceRef = *mut c_void;
pub type CGEventRef = *mut c_void;
pub type CGEventTapProxy = *mut c_void;
pub type CGDirectDisplayID = u32;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

pub type CGEventTapCallBack =
    unsafe extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

// CGEventTapLocation / CGEventTapPlacement / CGEventTapOptions
pub const kCGSessionEventTap: u32 = 1;
pub const kCGHeadInsertEventTap: u32 = 0;
pub const kCGEventTapOptionDefault: u32 = 0;

// CGEventType
pub const kCGEventLeftMouseDown: u32 = 1;
pub const kCGEventLeftMouseUp: u32 = 2;
pub const kCGEventRightMouseDown: u32 = 3;
pub const kCGEventRightMouseUp: u32 = 4;
pub const kCGEventMouseMoved: u32 = 5;
pub const kCGEventLeftMouseDragged: u32 = 6;
pub const kCGEventRightMouseDragged: u32 = 7;
pub const kCGEventKeyDown: u32 = 10;
pub const kCGEventKeyUp: u32 = 11;
pub const kCGEventFlagsChanged: u32 = 12;
pub const kCGEventScrollWheel: u32 = 22;
pub const kCGEventOtherMouseDown: u32 = 25;
pub const kCGEventOtherMouseUp: u32 = 26;
pub const kCGEventOtherMouseDragged: u32 = 27;
pub const kCGEventTapDisabledByTimeout: u32 = 0xFFFF_FFFE;
pub const kCGEventTapDisabledByUserInput: u32 = 0xFFFF_FFFF;
/// NSEventType gesture events (rotate, begin/end, gesture, magnify, swipe,
/// smart magnify, pressure). Not in CGEventType but delivered to taps.
pub const GESTURE_EVENT_TYPES: [u32; 8] = [18, 19, 20, 29, 30, 31, 32, 34];

// CGEventField
pub const kCGMouseEventButtonNumber: u32 = 3;
pub const kCGMouseEventDeltaX: u32 = 4;
pub const kCGMouseEventDeltaY: u32 = 5;
pub const kCGKeyboardEventKeycode: u32 = 9;
pub const kCGScrollWheelEventIsContinuous: u32 = 88;
pub const kCGScrollWheelEventFixedPtDeltaAxis1: u32 = 93;
pub const kCGScrollWheelEventFixedPtDeltaAxis2: u32 = 94;
pub const kCGScrollWheelEventPointDeltaAxis1: u32 = 96;
pub const kCGScrollWheelEventPointDeltaAxis2: u32 = 97;

// Device-dependent modifier bits (IOLLEvent.h NX_DEVICE*KEYMASK).
pub const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
pub const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
pub const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
pub const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
pub const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
pub const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
pub const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;
pub const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
/// `KBGetLayoutType` result for ISO keyboards ('ISO ').
pub const kKeyboardISO: u32 = u32::from_be_bytes(*b"ISO ");

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub static kCFRunLoopCommonModes: CFStringRef;
    pub static kCFBooleanTrue: CFTypeRef;
    pub static kCFTypeDictionaryKeyCallBacks: c_void;
    pub static kCFTypeDictionaryValueCallBacks: c_void;

    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFMachPortCreateRunLoopSource(
        allocator: CFTypeRef,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    pub fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    pub fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub fn CFRunLoopRun();
    pub fn CFStringCreateWithCString(
        allocator: CFTypeRef,
        cstr: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    pub fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    pub fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    pub fn CGEventGetFlags(event: CGEventRef) -> u64;
    pub fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    pub fn CGEventGetDoubleValueField(event: CGEventRef, field: u32) -> f64;

    pub fn CGGetActiveDisplayList(
        max: u32,
        displays: *mut CGDirectDisplayID,
        count: *mut u32,
    ) -> i32;
    pub fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    pub fn CGMainDisplayID() -> CGDirectDisplayID;

    pub fn CGWarpMouseCursorPosition(point: CGPoint) -> i32;
    pub fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
    pub fn CGDisplayHideCursor(display: CGDirectDisplayID) -> i32;
    pub fn CGDisplayShowCursor(display: CGDirectDisplayID) -> i32;

    pub fn CGPreflightListenEventAccess() -> bool;
    pub fn CGRequestListenEventAccess() -> bool;

    // Private but long-stable: lets a background process hide the cursor.
    pub fn _CGSDefaultConnection() -> i32;
    pub fn CGSSetConnectionProperty(
        cid: i32,
        target: i32,
        key: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    pub static kAXTrustedCheckOptionPrompt: CFStringRef;
    pub fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    pub fn LMGetKbdType() -> u8;
    pub fn KBGetLayoutType(keyboard_type: i16) -> u32;
}
