//! USB HID usage (page 0x07) → Windows scancode (set 1).
//!
//! Portable on purpose so the table is tested on every CI runner. Values match
//! what Windows itself reports in `KBDLLHOOKSTRUCT::scanCode` (plus the
//! extended flag for `E0`-prefixed keys).

use onemouse_protocol::KeyCode;
use onemouse_protocol::key::*;

/// How to inject one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyTarget {
    /// `KEYEVENTF_SCANCODE`, plus `KEYEVENTF_EXTENDEDKEY` when `extended`
    /// (the key's make code is `E0`-prefixed).
    Scan { code: u16, extended: bool },
    /// Inject by virtual-key code. Only for keys whose make code can't be
    /// expressed as a single scancode (Pause is `E1 1D 45`).
    Vk(u16),
}

pub const VK_PAUSE: u16 = 0x13;

const fn scan(code: u16) -> Option<KeyTarget> {
    Some(KeyTarget::Scan {
        code,
        extended: false,
    })
}

const fn ext(code: u16) -> Option<KeyTarget> {
    Some(KeyTarget::Scan {
        code,
        extended: true,
    })
}

pub fn lookup(key: KeyCode) -> Option<KeyTarget> {
    match key {
        A => scan(0x1E),
        B => scan(0x30),
        C => scan(0x2E),
        D => scan(0x20),
        E => scan(0x12),
        F => scan(0x21),
        G => scan(0x22),
        H => scan(0x23),
        I => scan(0x17),
        J => scan(0x24),
        K => scan(0x25),
        L => scan(0x26),
        M => scan(0x32),
        N => scan(0x31),
        O => scan(0x18),
        P => scan(0x19),
        Q => scan(0x10),
        R => scan(0x13),
        S => scan(0x1F),
        T => scan(0x14),
        U => scan(0x16),
        V => scan(0x2F),
        W => scan(0x11),
        X => scan(0x2D),
        Y => scan(0x15),
        Z => scan(0x2C),

        DIGIT_1 => scan(0x02),
        DIGIT_2 => scan(0x03),
        DIGIT_3 => scan(0x04),
        DIGIT_4 => scan(0x05),
        DIGIT_5 => scan(0x06),
        DIGIT_6 => scan(0x07),
        DIGIT_7 => scan(0x08),
        DIGIT_8 => scan(0x09),
        DIGIT_9 => scan(0x0A),
        DIGIT_0 => scan(0x0B),

        ENTER => scan(0x1C),
        ESCAPE => scan(0x01),
        BACKSPACE => scan(0x0E),
        TAB => scan(0x0F),
        SPACE => scan(0x39),
        MINUS => scan(0x0C),
        EQUAL => scan(0x0D),
        BRACKET_LEFT => scan(0x1A),
        BRACKET_RIGHT => scan(0x1B),
        BACKSLASH => scan(0x2B),
        // Non-US # (ISO/ABNT2 key next to Enter) shares the scancode.
        KeyCode(0x32) => scan(0x2B),
        SEMICOLON => scan(0x27),
        QUOTE => scan(0x28),
        BACKQUOTE => scan(0x29),
        COMMA => scan(0x33),
        PERIOD => scan(0x34),
        SLASH => scan(0x35),
        CAPS_LOCK => scan(0x3A),

        F1 => scan(0x3B),
        F2 => scan(0x3C),
        F3 => scan(0x3D),
        F4 => scan(0x3E),
        F5 => scan(0x3F),
        F6 => scan(0x40),
        F7 => scan(0x41),
        F8 => scan(0x42),
        F9 => scan(0x43),
        F10 => scan(0x44),
        F11 => scan(0x57),
        F12 => scan(0x58),

        PRINT_SCREEN => ext(0x37),
        SCROLL_LOCK => scan(0x46),
        PAUSE => Some(KeyTarget::Vk(VK_PAUSE)),
        INSERT => ext(0x52),
        HOME => ext(0x47),
        PAGE_UP => ext(0x49),
        DELETE => ext(0x53),
        END => ext(0x4F),
        PAGE_DOWN => ext(0x51),
        ARROW_RIGHT => ext(0x4D),
        ARROW_LEFT => ext(0x4B),
        ARROW_DOWN => ext(0x50),
        ARROW_UP => ext(0x48),

        // Windows reports Num Lock as E0 45 (and Pause as plain 45).
        NUM_LOCK => ext(0x45),
        NUMPAD_DIVIDE => ext(0x35),
        NUMPAD_MULTIPLY => scan(0x37),
        NUMPAD_SUBTRACT => scan(0x4A),
        NUMPAD_ADD => scan(0x4E),
        NUMPAD_ENTER => ext(0x1C),
        NUMPAD_1 => scan(0x4F),
        NUMPAD_2 => scan(0x50),
        NUMPAD_3 => scan(0x51),
        NUMPAD_4 => scan(0x4B),
        NUMPAD_5 => scan(0x4C),
        NUMPAD_6 => scan(0x4D),
        NUMPAD_7 => scan(0x47),
        NUMPAD_8 => scan(0x48),
        NUMPAD_9 => scan(0x49),
        NUMPAD_0 => scan(0x52),
        NUMPAD_DECIMAL => scan(0x53),

        INTL_BACKSLASH => scan(0x56),
        CONTEXT_MENU => ext(0x5D),
        NUMPAD_EQUAL => scan(0x59),
        F13 => scan(0x64),
        F14 => scan(0x65),
        F15 => scan(0x66),
        F16 => scan(0x67),
        F17 => scan(0x68),
        F18 => scan(0x69),
        F19 => scan(0x6A),
        F20 => scan(0x6B),
        MUTE => ext(0x20),
        VOLUME_UP => ext(0x30),
        VOLUME_DOWN => ext(0x2E),
        NUMPAD_COMMA => scan(0x7E),
        INTL_RO => scan(0x73),
        INTL_YEN => scan(0x7D),

        LEFT_CTRL => scan(0x1D),
        LEFT_SHIFT => scan(0x2A),
        LEFT_ALT => scan(0x38),
        LEFT_META => ext(0x5B),
        RIGHT_CTRL => ext(0x1D),
        RIGHT_SHIFT => scan(0x36),
        RIGHT_ALT => ext(0x38),
        RIGHT_META => ext(0x5C),

        _ => None,
    }
}

/// A low-level hook event (`KBDLLHOOKSTRUCT`) back to a physical key: the
/// scancode plus the extended (`E0`-prefix) flag. `vk` catches Pause, whose
/// make sequence (`E1 1D 45`) arrives as a plain `0x45` with `vk == VK_PAUSE`
/// (Num Lock is the extended `0x45`).
///
/// Returns `None` for keys with no mapping; the caller still swallows them
/// while remote so nothing leaks to local apps.
pub fn from_hook(vk: u16, scan: u16, extended: bool) -> Option<KeyCode> {
    if vk == VK_PAUSE {
        return Some(PAUSE);
    }
    Some(match (scan, extended) {
        (0x1E, false) => A,
        (0x30, false) => B,
        (0x2E, false) => C,
        (0x20, false) => D,
        (0x12, false) => E,
        (0x21, false) => F,
        (0x22, false) => G,
        (0x23, false) => H,
        (0x17, false) => I,
        (0x24, false) => J,
        (0x25, false) => K,
        (0x26, false) => L,
        (0x32, false) => M,
        (0x31, false) => N,
        (0x18, false) => O,
        (0x19, false) => P,
        (0x10, false) => Q,
        (0x13, false) => R,
        (0x1F, false) => S,
        (0x14, false) => T,
        (0x16, false) => U,
        (0x2F, false) => V,
        (0x11, false) => W,
        (0x2D, false) => X,
        (0x15, false) => Y,
        (0x2C, false) => Z,

        (0x02, false) => DIGIT_1,
        (0x03, false) => DIGIT_2,
        (0x04, false) => DIGIT_3,
        (0x05, false) => DIGIT_4,
        (0x06, false) => DIGIT_5,
        (0x07, false) => DIGIT_6,
        (0x08, false) => DIGIT_7,
        (0x09, false) => DIGIT_8,
        (0x0A, false) => DIGIT_9,
        (0x0B, false) => DIGIT_0,

        (0x1C, false) => ENTER,
        (0x01, false) => ESCAPE,
        (0x0E, false) => BACKSPACE,
        (0x0F, false) => TAB,
        (0x39, false) => SPACE,
        (0x0C, false) => MINUS,
        (0x0D, false) => EQUAL,
        (0x1A, false) => BRACKET_LEFT,
        (0x1B, false) => BRACKET_RIGHT,
        // 0x2B is also the ISO/ABNT2 key next to Enter (HID 0x32): same
        // finger, and the remote side can't tell them apart anyway.
        (0x2B, false) => BACKSLASH,
        (0x27, false) => SEMICOLON,
        (0x28, false) => QUOTE,
        (0x29, false) => BACKQUOTE,
        (0x33, false) => COMMA,
        (0x34, false) => PERIOD,
        (0x35, false) => SLASH,
        (0x3A, false) => CAPS_LOCK,

        (0x3B, false) => F1,
        (0x3C, false) => F2,
        (0x3D, false) => F3,
        (0x3E, false) => F4,
        (0x3F, false) => F5,
        (0x40, false) => F6,
        (0x41, false) => F7,
        (0x42, false) => F8,
        (0x43, false) => F9,
        (0x44, false) => F10,
        (0x57, false) => F11,
        (0x58, false) => F12,

        (0x37, true) => PRINT_SCREEN,
        (0x46, false) => SCROLL_LOCK,
        (0x52, true) => INSERT,
        (0x47, true) => HOME,
        (0x49, true) => PAGE_UP,
        (0x53, true) => DELETE,
        (0x4F, true) => END,
        (0x51, true) => PAGE_DOWN,
        (0x4D, true) => ARROW_RIGHT,
        (0x4B, true) => ARROW_LEFT,
        (0x50, true) => ARROW_DOWN,
        (0x48, true) => ARROW_UP,

        (0x45, true) => NUM_LOCK,
        (0x35, true) => NUMPAD_DIVIDE,
        (0x37, false) => NUMPAD_MULTIPLY,
        (0x4A, false) => NUMPAD_SUBTRACT,
        (0x4E, false) => NUMPAD_ADD,
        (0x1C, true) => NUMPAD_ENTER,
        (0x4F, false) => NUMPAD_1,
        (0x50, false) => NUMPAD_2,
        (0x51, false) => NUMPAD_3,
        (0x4B, false) => NUMPAD_4,
        (0x4C, false) => NUMPAD_5,
        (0x4D, false) => NUMPAD_6,
        (0x47, false) => NUMPAD_7,
        (0x48, false) => NUMPAD_8,
        (0x49, false) => NUMPAD_9,
        (0x52, false) => NUMPAD_0,
        (0x53, false) => NUMPAD_DECIMAL,

        (0x56, false) => INTL_BACKSLASH,
        (0x5D, true) => CONTEXT_MENU,
        (0x59, false) => NUMPAD_EQUAL,
        (0x64, false) => F13,
        (0x65, false) => F14,
        (0x66, false) => F15,
        (0x67, false) => F16,
        (0x68, false) => F17,
        (0x69, false) => F18,
        (0x6A, false) => F19,
        (0x6B, false) => F20,
        (0x20, true) => MUTE,
        (0x30, true) => VOLUME_UP,
        (0x2E, true) => VOLUME_DOWN,
        (0x7E, false) => NUMPAD_COMMA,
        (0x73, false) => INTL_RO,
        (0x7D, false) => INTL_YEN,

        (0x1D, false) => LEFT_CTRL,
        (0x2A, false) => LEFT_SHIFT,
        (0x38, false) => LEFT_ALT,
        (0x5B, true) => LEFT_META,
        (0x1D, true) => RIGHT_CTRL,
        (0x36, false) => RIGHT_SHIFT,
        (0x38, true) => RIGHT_ALT,
        (0x5C, true) => RIGHT_META,

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn every_protocol_key_is_mapped() {
        let missing: Vec<_> = ALL
            .iter()
            .filter(|(_, code)| lookup(*code).is_none())
            .map(|(name, _)| *name)
            .collect();
        assert!(missing.is_empty(), "unmapped keys: {missing:?}");
    }

    #[test]
    fn targets_are_unique() {
        let mut seen = HashMap::new();
        for (name, code) in ALL {
            let target = lookup(*code).unwrap();
            if let Some(other) = seen.insert(target, *name) {
                panic!("{name} and {other} both map to {target:?}");
            }
        }
    }

    #[test]
    fn hook_events_round_trip_through_lookup() {
        // Every key the injector knows comes back from a hook event,
        // except the ISO key sharing BACKSLASH's scancode (same finger).
        for (name, code) in ALL {
            if *code == KeyCode(0x32) {
                assert_eq!(from_hook(0, 0x2B, false), Some(BACKSLASH));
                continue;
            }
            let Some(target) = lookup(*code) else {
                panic!("{name} has no mapping");
            };
            let (vk, scan, extended) = match target {
                KeyTarget::Scan { code, extended } => (0u16, code, extended),
                KeyTarget::Vk(vk) => (vk, 0, false),
            };
            assert_eq!(from_hook(vk, scan, extended), Some(*code), "{name}");
        }
        assert_eq!(from_hook(VK_PAUSE, 0x45, false), Some(PAUSE));
        assert_eq!(from_hook(0, 0x45, true), Some(NUM_LOCK));
        assert_eq!(from_hook(0, 0x00, false), None);
        // Right modifiers differ from left only by the extended flag.
        assert_eq!(from_hook(0, 0x1D, false), Some(LEFT_CTRL));
        assert_eq!(from_hook(0, 0x1D, true), Some(RIGHT_CTRL));
    }

    #[test]
    fn extended_keys() {
        assert_eq!(lookup(ARROW_UP), ext(0x48));
        assert_eq!(lookup(NUMPAD_8), scan(0x48));
        assert_eq!(lookup(RIGHT_ALT), ext(0x38));
        assert_eq!(lookup(NUMPAD_ENTER), ext(0x1C));
        assert_eq!(lookup(PAUSE), Some(KeyTarget::Vk(VK_PAUSE)));
        assert_eq!(lookup(KeyCode(0xFFFF)), None);
    }
}
