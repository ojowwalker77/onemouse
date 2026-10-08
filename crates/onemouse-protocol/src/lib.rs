//! Wire protocol shared by the onemouse **primary** (macOS, owns the keyboard
//! and trackpad) and **secondary** (Windows, receives and injects input).
//!
//! The secondary connects to the primary over TCP, both sides establish the
//! encrypted `onemouse-transport` channel, then exchange length-prefixed
//! [`Message`] frames (see [`frame`]) inside it. See `docs/PROTOCOL.md`
//! for the conversation flow.
//!
//! # Compatibility rules
//!
//! Messages are encoded with `postcard`, which identifies enum variants by
//! position. Therefore:
//! - never reorder or remove variants or fields; only append new variants at
//!   the end of an enum,
//! - bump [`PROTOCOL_VERSION`] on any change to the encoded form,
//! - [`Message::Hello`] must stay variant 0 with `protocol_version` as its
//!   first field, so any version can read it and reject a mismatch cleanly.

pub mod frame;
pub mod key;

use serde::{Deserialize, Serialize};

pub use frame::{FrameError, MAX_FRAME_LEN, decode, encode, read_message, write_message};
pub use key::KeyCode;

/// Wire protocol version. Both sides must match exactly.
///
/// - 1: plaintext TCP (M1, dev only).
/// - 2: the same messages inside the `onemouse-transport` channel (Noise XX,
///   pinned keys, pairing). A v1 peer can't complete the handshake, so a
///   mismatch shows up as a failed handshake rather than a `Reject`.
pub const PROTOCOL_VERSION: u16 = 2;

/// TCP port the primary listens on.
pub const DEFAULT_PORT: u16 = 24801;

/// mDNS service type the primary advertises (M2).
pub const SERVICE_TYPE: &str = "_onemouse._tcp.local.";

/// Scroll units per wheel notch (same as Windows `WHEEL_DELTA`).
pub const SCROLL_UNITS_PER_NOTCH: i32 = 120;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// secondary → primary, first message on every connection.
    Hello(Hello),
    /// primary → secondary, accepts the `Hello`.
    Welcome {
        protocol_version: u16,
        name: String,
    },
    /// Either side, then close the connection.
    Reject {
        reason: String,
    },
    /// secondary → primary, whenever the display layout changes.
    DisplaysChanged {
        displays: Vec<Display>,
    },
    /// primary → secondary. The cursor now belongs to the secondary and starts
    /// at (`x`, `y`). The secondary must assume no keys or buttons are held.
    Enter {
        x: i32,
        y: i32,
    },
    /// primary → secondary. The cursor went back to the primary; release every
    /// key and button still held from this session.
    Leave,
    /// primary → secondary. Absolute position in the secondary's virtual
    /// desktop, physical pixels.
    MouseMove {
        x: i32,
        y: i32,
    },
    /// primary → secondary.
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// primary → secondary. In [`SCROLL_UNITS_PER_NOTCH`] units; `dy > 0`
    /// scrolls up (away from the user), `dx > 0` scrolls right.
    Scroll {
        dx: i32,
        dy: i32,
    },
    /// primary → secondary. Final key after any modifier remapping.
    Key {
        code: KeyCode,
        pressed: bool,
    },
    /// Either side; the other answers with `Pong` carrying the same value.
    Ping(u64),
    Pong(u64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    /// Human-readable machine name, shown in the arrange UI.
    pub name: String,
    pub os: Os,
    pub displays: Vec<Display>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    MacOs,
    Windows,
    Linux,
}

/// One monitor of the secondary, in its virtual desktop coordinates
/// (physical pixels; `x`/`y` may be negative).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Display {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// DPI scale factor, e.g. 1.5 for 150 %.
    pub scale: f32,
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}
