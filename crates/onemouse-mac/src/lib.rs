//! onemouse primary for macOS: captures the keyboard and trackpad, and when
//! the cursor crosses into the secondary's screen, sends the input there.
//!
//! Geometry, keymap, input routing and networking are portable and tested on
//! every platform; the event tap and cursor control are behind `cfg(macos)`.
//!
//! Until M2 (Noise encryption) this is plaintext: dev use on a trusted LAN only.

pub mod config;
pub mod controller;
pub mod keymap;
pub mod layout;
pub mod pairing;
pub mod server;

#[cfg(target_os = "macos")]
pub mod macos;

#[macro_export]
#[doc(hidden)]
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("[onemouse-mac] {}", format_args!($($arg)*))
    };
}
