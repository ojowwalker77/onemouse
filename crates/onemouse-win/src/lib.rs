//! onemouse secondary for Windows: connects to the primary (the Mac), reports
//! this PC's displays and injects the keyboard and mouse input it receives.
//!
//! The protocol handling, key table and held-key bookkeeping are portable and
//! tested on every platform; the Win32 parts are behind `cfg(windows)`.
//!
//! Until M2 (Noise encryption) this is plaintext: dev use on a trusted LAN only.

pub mod client;
pub mod inject;
pub mod keymap;

#[cfg(windows)]
pub mod display;
#[cfg(windows)]
pub mod sendinput;

#[macro_export]
#[doc(hidden)]
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("[onemouse-win] {}", format_args!($($arg)*))
    };
}

/// The real machine: Win32 display enumeration and `WM_DISPLAYCHANGE`.
#[cfg(windows)]
#[derive(Debug)]
pub struct WindowsHost {
    pub name: String,
}

#[cfg(windows)]
impl client::Host for WindowsHost {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn displays(&self) -> Vec<onemouse_protocol::Display> {
        display::displays()
    }

    fn display_generation(&self) -> u64 {
        display::generation()
    }
}
