//! onemouse secondary for Windows: connects to the primary (the Mac), reports
//! this PC's displays and injects the keyboard and mouse input it receives.
//!
//! The protocol handling, key table and held-key bookkeeping are portable and
//! tested on every platform; the Win32 parts are behind `cfg(windows)`.
//!
//! The connection is encrypted and authenticated with `onemouse-transport`:
//! the first connection pairs (both screens show the same 6-digit code), later
//! ones reconnect silently.

pub mod capture;
pub mod client;
pub mod inject;
pub mod keymap;
pub mod logging;

#[cfg(windows)]
pub mod autostart;
#[cfg(windows)]
pub mod display;
#[cfg(windows)]
pub mod sendinput;
#[cfg(windows)]
pub mod tray;

#[macro_export]
#[doc(hidden)]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::logging::write(format_args!($($arg)*))
    };
}

/// The real machine: Win32 display enumeration and `WM_DISPLAYCHANGE`.
#[cfg(windows)]
#[derive(Debug)]
pub struct WindowsHost {
    pub name: String,
    /// The `arrangement` file, shared with `onemouse-core`: persists the
    /// "keyboard & mouse are on" setting. `None` keeps the default.
    pub config_path: Option<std::path::PathBuf>,
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

    fn status(&self, status: &str) {
        tray::set_status(status);
    }

    fn main_setting(&self) -> onemouse_protocol::Main {
        self.config_path
            .as_deref()
            .map(onemouse_core::config::Config::load)
            .map(|config| config.main_or_default())
            .unwrap_or(onemouse_protocol::Main::Server)
    }

    fn set_main_setting(&self, main: onemouse_protocol::Main) {
        let Some(path) = self.config_path.as_deref() else {
            return;
        };
        let mut config = onemouse_core::config::Config::load(path);
        if config.main == Some(main) {
            return;
        }
        config.main = Some(main);
        if let Err(e) = config.save(path) {
            log!("couldn't save the main setting to {}: {e}", path.display());
        }
    }
}
