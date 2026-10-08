//! onemouse primary for macOS: captures the keyboard and trackpad, and when
//! the cursor crosses into the secondary's screen, sends the input there.
//!
//! Geometry, keymap, input routing and networking are portable and tested on
//! every platform; the event tap and cursor control are behind `cfg(macos)`.
//!
//! Until M2 (Noise encryption) this is plaintext: dev use on a trusted LAN only.

pub mod config;
pub mod controller;
pub mod install;
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
        eprintln!("[onemouse-mac {}] {}", $crate::clock(), format_args!($($arg)*))
    };
}

/// `HH:MM:SSZ` (UTC) for log lines, which end up in a file when running at
/// login.
#[doc(hidden)]
pub fn clock() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}Z", day / 3600, day % 3600 / 60, day % 60)
}
