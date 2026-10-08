//! Input injection with bookkeeping of everything held down, so it can all be
//! released when the cursor leaves or the connection drops.
//!
//! The OS-specific part is the [`Backend`]; on Windows that's
//! [`crate::sendinput::SendInputBackend`].

use std::collections::BTreeMap;
use std::io;

use onemouse_protocol::{KeyCode, MouseButton};

use crate::keymap::{self, KeyTarget};

/// Raw, stateless injection primitives.
pub trait Backend {
    /// Absolute position in virtual-desktop physical pixels.
    fn move_to(&mut self, x: i32, y: i32) -> io::Result<()>;
    fn button(&mut self, button: MouseButton, pressed: bool) -> io::Result<()>;
    /// Vertical wheel, 120 per notch, positive = away from the user.
    fn wheel(&mut self, delta: i32) -> io::Result<()>;
    /// Horizontal wheel, 120 per notch, positive = right.
    fn hwheel(&mut self, delta: i32) -> io::Result<()>;
    fn key(&mut self, key: KeyTarget, pressed: bool) -> io::Result<()>;
}

const BUTTONS: [MouseButton; 5] = [
    MouseButton::Left,
    MouseButton::Right,
    MouseButton::Middle,
    MouseButton::Back,
    MouseButton::Forward,
];

fn button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

pub struct Injector<B> {
    backend: B,
    /// Keys we pressed and haven't released, with how we injected them.
    keys: BTreeMap<KeyCode, KeyTarget>,
    buttons: [bool; 5],
}

impl<B: Backend> Injector<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            keys: BTreeMap::new(),
            buttons: [false; 5],
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn move_to(&mut self, x: i32, y: i32) -> io::Result<()> {
        self.backend.move_to(x, y)
    }

    /// Releases of buttons we never pressed are dropped, so a stray "up"
    /// can't end a drag the local user started.
    pub fn button(&mut self, button: MouseButton, pressed: bool) -> io::Result<()> {
        let held = &mut self.buttons[button_index(button)];
        if !pressed && !*held {
            return Ok(());
        }
        *held = pressed;
        self.backend.button(button, pressed)
    }

    pub fn scroll(&mut self, dx: i32, dy: i32) -> io::Result<()> {
        if dy != 0 {
            self.backend.wheel(dy)?;
        }
        if dx != 0 {
            self.backend.hwheel(dx)?;
        }
        Ok(())
    }

    /// A press of a key that is already down is injected again: that's how
    /// autorepeat reaches Windows. Releases of keys we never pressed are
    /// dropped. Keys without a Windows mapping are reported as `Unsupported`.
    pub fn key(&mut self, code: KeyCode, pressed: bool) -> io::Result<()> {
        if pressed {
            let target = keymap::lookup(code).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("no Windows mapping for HID usage {:#04x}", code.0),
                )
            })?;
            self.keys.insert(code, target);
            self.backend.key(target, true)
        } else {
            match self.keys.remove(&code) {
                Some(target) => self.backend.key(target, false),
                None => Ok(()),
            }
        }
    }

    pub fn is_idle(&self) -> bool {
        self.keys.is_empty() && !self.buttons.contains(&true)
    }

    /// Releases every key and button still held. Never fails: every release is
    /// attempted and the state is cleared even if the OS refuses some.
    pub fn release_all(&mut self) {
        // Modifiers last, so e.g. releasing Ctrl+C doesn't leave a lone C.
        let (modifiers, others): (Vec<_>, Vec<_>) = std::mem::take(&mut self.keys)
            .into_iter()
            .partition(|(code, _)| code.is_modifier());
        for (_, target) in others.into_iter().chain(modifiers) {
            let _ = self.backend.key(target, false);
        }
        for button in BUTTONS {
            let held = &mut self.buttons[button_index(button)];
            if std::mem::take(held) {
                let _ = self.backend.button(button, false);
            }
        }
    }
}

/// A [`Backend`] that records calls, for tests on any platform.
#[derive(Debug, Default, Clone)]
pub struct RecordingBackend {
    pub events: Vec<Recorded>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Recorded {
    Move(i32, i32),
    Button(MouseButton, bool),
    Wheel(i32),
    HWheel(i32),
    Key(KeyTarget, bool),
}

impl Backend for RecordingBackend {
    fn move_to(&mut self, x: i32, y: i32) -> io::Result<()> {
        self.events.push(Recorded::Move(x, y));
        Ok(())
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> io::Result<()> {
        self.events.push(Recorded::Button(button, pressed));
        Ok(())
    }

    fn wheel(&mut self, delta: i32) -> io::Result<()> {
        self.events.push(Recorded::Wheel(delta));
        Ok(())
    }

    fn hwheel(&mut self, delta: i32) -> io::Result<()> {
        self.events.push(Recorded::HWheel(delta));
        Ok(())
    }

    fn key(&mut self, key: KeyTarget, pressed: bool) -> io::Result<()> {
        self.events.push(Recorded::Key(key, pressed));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onemouse_protocol::key;

    fn t(code: KeyCode) -> KeyTarget {
        keymap::lookup(code).unwrap()
    }

    #[test]
    fn release_all_releases_everything_held_once() {
        let mut inj = Injector::new(RecordingBackend::default());
        inj.key(key::LEFT_CTRL, true).unwrap();
        inj.key(key::C, true).unwrap();
        inj.key(key::A, true).unwrap();
        inj.key(key::A, false).unwrap();
        inj.button(MouseButton::Left, true).unwrap();
        inj.button(MouseButton::Back, true).unwrap();
        assert!(!inj.is_idle());

        inj.backend.events.clear();
        inj.release_all();
        assert_eq!(
            inj.backend().events,
            [
                Recorded::Key(t(key::C), false),
                Recorded::Key(t(key::LEFT_CTRL), false),
                Recorded::Button(MouseButton::Left, false),
                Recorded::Button(MouseButton::Back, false),
            ]
        );
        assert!(inj.is_idle());

        inj.backend.events.clear();
        inj.release_all();
        assert!(inj.backend().events.is_empty());
    }

    #[test]
    fn autorepeat_is_injected_and_stray_releases_are_dropped() {
        let mut inj = Injector::new(RecordingBackend::default());
        inj.key(key::BACKSPACE, true).unwrap();
        inj.key(key::BACKSPACE, true).unwrap();
        inj.key(key::BACKSPACE, false).unwrap();
        inj.key(key::BACKSPACE, false).unwrap();
        inj.button(MouseButton::Right, false).unwrap();
        let bs = t(key::BACKSPACE);
        assert_eq!(
            inj.backend().events,
            [
                Recorded::Key(bs, true),
                Recorded::Key(bs, true),
                Recorded::Key(bs, false),
            ]
        );
    }

    #[test]
    fn scroll_splits_axes() {
        let mut inj = Injector::new(RecordingBackend::default());
        inj.scroll(0, 120).unwrap();
        inj.scroll(-30, 0).unwrap();
        inj.scroll(5, -240).unwrap();
        inj.scroll(0, 0).unwrap();
        assert_eq!(
            inj.backend().events,
            [
                Recorded::Wheel(120),
                Recorded::HWheel(-30),
                Recorded::Wheel(-240),
                Recorded::HWheel(5),
            ]
        );
    }

    #[test]
    fn unmapped_keys_are_rejected_without_tracking() {
        let mut inj = Injector::new(RecordingBackend::default());
        let err = inj.key(KeyCode(0x01), true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(inj.is_idle());
    }
}
