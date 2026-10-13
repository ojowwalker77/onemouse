//! onemouse primary for macOS: captures the keyboard and trackpad, and when
//! the cursor crosses into the secondary's screen, sends the input there.
//!
//! Geometry, input routing and translation live in `onemouse-core`; the
//! keymap and networking here are portable too. The event tap, cursor
//! control and menu-bar UI are behind `cfg(macos)`.

pub mod inject;
pub mod install;
pub mod keymap;
pub mod pairing;
pub mod server;

#[cfg(target_os = "macos")]
pub mod macos;

// Moved to onemouse-core; re-exported so paths stay `onemouse_mac::layout` etc.
pub use onemouse_core::{config, controller, layout};

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
