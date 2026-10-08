//! onemouse primary for macOS: captures the keyboard and trackpad, and when
//! the cursor crosses into the secondary's screen, sends the input there.
//!
//! Geometry, input routing and translation live in `onemouse-core`; the
//! keymap and networking here are portable too. The event tap, cursor
//! control and menu-bar UI are behind `cfg(macos)`.

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
        eprintln!("[onemouse-mac] {}", format_args!($($arg)*))
    };
}
