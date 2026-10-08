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

const VK_PAUSE: u16 = 0x13;

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
    fn extended_keys() {
        assert_eq!(lookup(ARROW_UP), ext(0x48));
        assert_eq!(lookup(NUMPAD_8), scan(0x48));
        assert_eq!(lookup(RIGHT_ALT), ext(0x38));
        assert_eq!(lookup(NUMPAD_ENTER), ext(0x1C));
        assert_eq!(lookup(PAUSE), Some(KeyTarget::Vk(VK_PAUSE)));
        assert_eq!(lookup(KeyCode(0xFFFF)), None);
    }
}
