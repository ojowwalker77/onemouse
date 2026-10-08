//! Keys are identified by their USB HID usage on the Keyboard/Keypad page
//! (0x07), see "HID Usage Tables", section 10. Each platform maps its native
//! codes to and from these: macOS virtual keycodes on the primary, Windows
//! scancodes on the secondary.
//!
//! Names follow the physical US layout position, not the character produced.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct KeyCode(pub u16);

impl KeyCode {
    pub const fn is_modifier(self) -> bool {
        self.0 >= LEFT_CTRL.0 && self.0 <= RIGHT_META.0
    }
}

macro_rules! keys {
    ($($(#[$meta:meta])* $name:ident = $code:literal,)*) => {
        $($(#[$meta])* pub const $name: KeyCode = KeyCode($code);)*

        /// Every named key, for building platform mapping tables and tests.
        pub const ALL: &[(&str, KeyCode)] = &[$((stringify!($name), $name),)*];
    };
}

keys! {
    A = 0x04, B = 0x05, C = 0x06, D = 0x07, E = 0x08, F = 0x09, G = 0x0A,
    H = 0x0B, I = 0x0C, J = 0x0D, K = 0x0E, L = 0x0F, M = 0x10, N = 0x11,
    O = 0x12, P = 0x13, Q = 0x14, R = 0x15, S = 0x16, T = 0x17, U = 0x18,
    V = 0x19, W = 0x1A, X = 0x1B, Y = 0x1C, Z = 0x1D,

    DIGIT_1 = 0x1E, DIGIT_2 = 0x1F, DIGIT_3 = 0x20, DIGIT_4 = 0x21,
    DIGIT_5 = 0x22, DIGIT_6 = 0x23, DIGIT_7 = 0x24, DIGIT_8 = 0x25,
    DIGIT_9 = 0x26, DIGIT_0 = 0x27,

    ENTER = 0x28, ESCAPE = 0x29, BACKSPACE = 0x2A, TAB = 0x2B, SPACE = 0x2C,
    MINUS = 0x2D, EQUAL = 0x2E, BRACKET_LEFT = 0x2F, BRACKET_RIGHT = 0x30,
    BACKSLASH = 0x31, SEMICOLON = 0x33, QUOTE = 0x34, BACKQUOTE = 0x35,
    COMMA = 0x36, PERIOD = 0x37, SLASH = 0x38, CAPS_LOCK = 0x39,

    F1 = 0x3A, F2 = 0x3B, F3 = 0x3C, F4 = 0x3D, F5 = 0x3E, F6 = 0x3F,
    F7 = 0x40, F8 = 0x41, F9 = 0x42, F10 = 0x43, F11 = 0x44, F12 = 0x45,

    PRINT_SCREEN = 0x46, SCROLL_LOCK = 0x47, PAUSE = 0x48, INSERT = 0x49,
    HOME = 0x4A, PAGE_UP = 0x4B, DELETE = 0x4C, END = 0x4D, PAGE_DOWN = 0x4E,
    ARROW_RIGHT = 0x4F, ARROW_LEFT = 0x50, ARROW_DOWN = 0x51, ARROW_UP = 0x52,

    NUM_LOCK = 0x53, NUMPAD_DIVIDE = 0x54, NUMPAD_MULTIPLY = 0x55,
    NUMPAD_SUBTRACT = 0x56, NUMPAD_ADD = 0x57, NUMPAD_ENTER = 0x58,
    NUMPAD_1 = 0x59, NUMPAD_2 = 0x5A, NUMPAD_3 = 0x5B, NUMPAD_4 = 0x5C,
    NUMPAD_5 = 0x5D, NUMPAD_6 = 0x5E, NUMPAD_7 = 0x5F, NUMPAD_8 = 0x60,
    NUMPAD_9 = 0x61, NUMPAD_0 = 0x62, NUMPAD_DECIMAL = 0x63,

    /// The extra key next to left Shift on ISO keyboards (§ on Mac ISO).
    INTL_BACKSLASH = 0x64,
    CONTEXT_MENU = 0x65,
    NUMPAD_EQUAL = 0x67,
    F13 = 0x68, F14 = 0x69, F15 = 0x6A, F16 = 0x6B, F17 = 0x6C, F18 = 0x6D,
    F19 = 0x6E, F20 = 0x6F,
    MUTE = 0x7F, VOLUME_UP = 0x80, VOLUME_DOWN = 0x81,
    NUMPAD_COMMA = 0x85,
    /// ABNT2 "/?" key (Brazilian layout).
    INTL_RO = 0x87,
    INTL_YEN = 0x89,

    LEFT_CTRL = 0xE0, LEFT_SHIFT = 0xE1, LEFT_ALT = 0xE2,
    /// Cmd on Mac, Windows key on PC.
    LEFT_META = 0xE3,
    RIGHT_CTRL = 0xE4, RIGHT_SHIFT = 0xE5, RIGHT_ALT = 0xE6, RIGHT_META = 0xE7,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_are_unique() {
        let codes: HashSet<_> = ALL.iter().map(|(_, code)| *code).collect();
        assert_eq!(codes.len(), ALL.len());
    }

    #[test]
    fn modifiers() {
        assert!(LEFT_META.is_modifier());
        assert!(RIGHT_SHIFT.is_modifier());
        assert!(!CAPS_LOCK.is_modifier());
        assert!(!SPACE.is_modifier());
    }
}
