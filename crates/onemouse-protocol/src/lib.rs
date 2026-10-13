//! Wire protocol shared by the two onemouse machines.
//!
//! Two independent roles:
//! - **server / client**: the server (the Mac) listens, the client (the PC)
//!   connects. Fixed.
//! - **main / remote**: the main side has the keyboard and mouse the user is
//!   using and sends input; the remote side injects it. Chosen by the user
//!   ([`Main`]), either way round, and can change at any time.
//!
//! The client connects, both sides establish the encrypted
//! `onemouse-transport` channel, then exchange length-prefixed [`Message`]
//! frames (see [`frame`]) inside it. See `docs/PROTOCOL.md` for the flow.
//!
//! # Compatibility rules
//!
//! Messages are encoded with `postcard`, which identifies enum variants by
//! position. Therefore:
//! - never reorder or remove variants or fields; only append new variants at
//!   the end of an enum, or new fields at the end of a variant (an older
//!   peer still decodes the leading fields: postcard ignores trailing bytes),
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
/// - 3: either side can be main ([`Main`], [`Message::SetMain`],
///   [`Message::Arrangement`]); `Welcome` describes the server.
pub const PROTOCOL_VERSION: u16 = 3;

/// TCP port the primary listens on.
pub const DEFAULT_PORT: u16 = 24801;

/// mDNS service type the primary advertises (M2).
pub const SERVICE_TYPE: &str = "_onemouse._tcp.local.";

/// Scroll units per wheel notch (same as Windows `WHEEL_DELTA`).
pub const SCROLL_UNITS_PER_NOTCH: i32 = 120;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// client → server, first message on every connection.
    Hello(Hello),
    /// server → client, accepts the `Hello`. Since v3 it also describes the
    /// server and says which side is main.
    Welcome {
        protocol_version: u16,
        name: String,
        /// v3: the server's OS, so the client knows how to translate.
        os: Os,
        /// v3: the server's displays (needed when the client is main).
        displays: Vec<Display>,
        /// v3: which side has the keyboard and mouse right now. The client
        /// adopts it (the server's setting is authoritative at connect).
        main: Main,
    },
    /// Either side, then close the connection.
    Reject {
        reason: String,
    },
    /// Either side, whenever its display layout changes (v2: client only).
    DisplaysChanged {
        displays: Vec<Display>,
    },
    /// main → remote. The cursor now belongs to the remote side and starts
    /// at (`x`, `y`). The remote side must assume no keys or buttons are held.
    Enter {
        x: i32,
        y: i32,
    },
    /// main → remote. The cursor went back to the main side; release every
    /// key and button still held from this session.
    Leave,
    /// main → remote. Absolute position in the remote side's own global
    /// desktop coordinates, in the units of its `Display`s (Windows: physical
    /// pixels; macOS: points).
    MouseMove {
        x: i32,
        y: i32,
    },
    /// main → remote.
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// main → remote. In [`SCROLL_UNITS_PER_NOTCH`] units; `dy > 0`
    /// scrolls up (away from the user), `dx > 0` scrolls right.
    Scroll {
        dx: i32,
        dy: i32,
    },
    /// main → remote. The key the remote side should press, i.e. after the
    /// main side translated shortcuts to the remote side's OS.
    Key {
        code: KeyCode,
        pressed: bool,
    },
    /// Either side; the other answers with `Pong` carrying the same value.
    Ping(u64),
    Pong(u64),
    /// v3, either side: the user chose which machine has the keyboard and
    /// mouse. The receiver adopts and persists it; nobody echoes it. A side
    /// that stops being main while the cursor is on the remote side sends
    /// `Leave` first.
    SetMain {
        main: Main,
    },
    /// v3, server → client, after `Welcome` and whenever the user rearranges
    /// the displays: the client's desktop origin `(0, 0)` sits at (`x`, `y`)
    /// in the server's global desktop coordinates, in the server's units.
    /// Lets either side run edge detection when it is main.
    Arrangement {
        x: i32,
        y: i32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    /// Human-readable machine name, shown in the arrange UI.
    pub name: String,
    pub os: Os,
    pub displays: Vec<Display>,
}

/// Which end of the connection has the keyboard and mouse ("main"); the
/// other end receives input ("remote"). Independent of who listens: the
/// server listens and the client connects, whichever is main.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Main {
    Server,
    Client,
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
