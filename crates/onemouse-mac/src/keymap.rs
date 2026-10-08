//! macOS virtual keycodes (`kVK_*`, Carbon `Events.h`) → USB HID usages, and
//! the modifier remapping applied before keys are sent to the secondary.
//!
//! Portable on purpose so the table is tested on every CI runner.

use onemouse_protocol::KeyCode;
use onemouse_protocol::key::*;

/// Maps a macOS virtual keycode to the HID usage of the same physical key.
///
/// Apple ISO keyboards report the top-left key as `kVK_ISO_Section` and the
/// key next to left Shift as `kVK_ANSI_Grave`, swapped relative to their
/// physical positions; `iso` undoes that.
pub fn from_mac(vk: u16, iso: bool) -> Option<KeyCode> {
    Some(match vk {
        0x00 => A,
        0x01 => S,
        0x02 => D,
        0x03 => F,
        0x04 => H,
        0x05 => G,
        0x06 => Z,
        0x07 => X,
        0x08 => C,
        0x09 => V,
        0x0A if iso => BACKQUOTE,
        0x0A => INTL_BACKSLASH,
        0x0B => B,
        0x0C => Q,
        0x0D => W,
        0x0E => E,
        0x0F => R,
        0x10 => Y,
        0x11 => T,
        0x12 => DIGIT_1,
        0x13 => DIGIT_2,
        0x14 => DIGIT_3,
        0x15 => DIGIT_4,
        0x16 => DIGIT_6,
        0x17 => DIGIT_5,
        0x18 => EQUAL,
        0x19 => DIGIT_9,
        0x1A => DIGIT_7,
        0x1B => MINUS,
        0x1C => DIGIT_8,
        0x1D => DIGIT_0,
        0x1E => BRACKET_RIGHT,
        0x1F => O,
        0x20 => U,
        0x21 => BRACKET_LEFT,
        0x22 => I,
        0x23 => P,
        0x24 => ENTER,
        0x25 => L,
        0x26 => J,
        0x27 => QUOTE,
        0x28 => K,
        0x29 => SEMICOLON,
        0x2A => BACKSLASH,
        0x2B => COMMA,
        0x2C => SLASH,
        0x2D => N,
        0x2E => M,
        0x2F => PERIOD,
        0x30 => TAB,
        0x31 => SPACE,
        0x32 if iso => INTL_BACKSLASH,
        0x32 => BACKQUOTE,
        0x33 => BACKSPACE,
        0x35 => ESCAPE,
        0x36 => RIGHT_META,
        0x37 => LEFT_META,
        0x38 => LEFT_SHIFT,
        0x39 => CAPS_LOCK,
        0x3A => LEFT_ALT,
        0x3B => LEFT_CTRL,
        0x3C => RIGHT_SHIFT,
        0x3D => RIGHT_ALT,
        0x3E => RIGHT_CTRL,
        0x40 => F17,
        0x41 => NUMPAD_DECIMAL,
        0x43 => NUMPAD_MULTIPLY,
        0x45 => NUMPAD_ADD,
        0x47 => NUM_LOCK, // Keypad Clear sits where Num Lock is.
        0x48 => VOLUME_UP,
        0x49 => VOLUME_DOWN,
        0x4A => MUTE,
        0x4B => NUMPAD_DIVIDE,
        0x4C => NUMPAD_ENTER,
        0x4E => NUMPAD_SUBTRACT,
        0x4F => F18,
        0x50 => F19,
        0x51 => NUMPAD_EQUAL,
        0x52 => NUMPAD_0,
        0x53 => NUMPAD_1,
        0x54 => NUMPAD_2,
        0x55 => NUMPAD_3,
        0x56 => NUMPAD_4,
        0x57 => NUMPAD_5,
        0x58 => NUMPAD_6,
        0x59 => NUMPAD_7,
        0x5A => F20,
        0x5B => NUMPAD_8,
        0x5C => NUMPAD_9,
        0x5D => INTL_YEN,
        0x5E => INTL_RO,
        0x5F => NUMPAD_COMMA,
        0x60 => F5,
        0x61 => F6,
        0x62 => F7,
        0x63 => F3,
        0x64 => F8,
        0x65 => F9,
        0x67 => F11,
        0x69 => F13,
        0x6A => F16,
        0x6B => F14,
        0x6D => F10,
        0x6E => CONTEXT_MENU,
        0x6F => F12,
        0x71 => F15,
        0x72 => INSERT, // Help sits where Insert is.
        0x73 => HOME,
        0x74 => PAGE_UP,
        0x75 => DELETE,
        0x76 => F4,
        0x77 => END,
        0x78 => F2,
        0x79 => PAGE_DOWN,
        0x7A => F1,
        0x7B => ARROW_LEFT,
        0x7C => ARROW_RIGHT,
        0x7D => ARROW_DOWN,
        0x7E => ARROW_UP,
        _ => return None,
    })
}

/// What a physical Mac key becomes on the secondary, so shortcuts keep their
/// muscle memory: Cmd → Ctrl (Cmd+C copies), Ctrl → Windows key. Option is
/// already Alt.
pub fn to_secondary(key: KeyCode) -> KeyCode {
    match key {
        LEFT_META => LEFT_CTRL,
        RIGHT_META => RIGHT_CTRL,
        LEFT_CTRL => LEFT_META,
        RIGHT_CTRL => RIGHT_META,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn maps_each_keycode_to_a_distinct_key() {
        for iso in [false, true] {
            let mapped: Vec<_> = (0..=0x7F).filter_map(|vk| from_mac(vk, iso)).collect();
            let unique: HashSet<_> = mapped.iter().collect();
            assert_eq!(unique.len(), mapped.len(), "iso={iso}");
        }
    }

    #[test]
    fn covers_every_key_a_mac_keyboard_has() {
        let mapped: HashSet<_> = (0..=0x7F).filter_map(|vk| from_mac(vk, false)).collect();
        let missing: Vec<_> = ALL
            .iter()
            .filter(|(_, code)| !mapped.contains(code))
            .map(|(name, _)| *name)
            .collect();
        // No Mac keyboard has these.
        assert_eq!(
            missing,
            ["PRINT_SCREEN", "SCROLL_LOCK", "PAUSE"],
            "unmapped keys"
        );
    }

    #[test]
    fn iso_swaps_section_and_grave() {
        assert_eq!(from_mac(0x0A, true), Some(BACKQUOTE));
        assert_eq!(from_mac(0x32, true), Some(INTL_BACKSLASH));
        assert_eq!(from_mac(0x32, false), Some(BACKQUOTE));
    }

    #[test]
    fn remaps_cmd_to_ctrl_and_back() {
        assert_eq!(to_secondary(LEFT_META), LEFT_CTRL);
        assert_eq!(to_secondary(RIGHT_CTRL), RIGHT_META);
        assert_eq!(to_secondary(LEFT_ALT), LEFT_ALT);
        assert_eq!(to_secondary(A), A);
    }
}
