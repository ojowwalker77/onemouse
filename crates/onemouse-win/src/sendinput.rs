//! [`Backend`] built on `SendInput`.

use std::io;
use std::mem;

use onemouse_protocol::MouseButton;
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SetCursorPos, XBUTTON1, XBUTTON2,
};

use crate::inject::Backend;
use crate::keymap::KeyTarget;

/// Tags our events (`dwExtraInfo`) so hooks can tell them from real input.
pub const EXTRA_INFO: usize = 0x4F4E_454D; // "ONEM"

#[derive(Debug, Default)]
pub struct SendInputBackend;

fn send(input: INPUT) -> io::Result<()> {
    // SAFETY: one fully initialised INPUT, correct cbSize.
    let sent = unsafe { SendInput(1, &input, mem::size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        // Usually UIPI: the foreground window is elevated and we aren't.
        Err(io::Error::last_os_error())
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> io::Result<()> {
    send(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as _,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: EXTRA_INFO,
            },
        },
    })
}

/// Maps `pos` within `[origin, origin + size)` to the 0..=65535 range of
/// `MOUSEEVENTF_ABSOLUTE`, rounding up so Windows (which truncates
/// `n * size / 65536`) lands exactly on `pos`.
fn normalize(pos: i32, origin: i32, size: i32) -> i32 {
    if size <= 0 {
        return 0;
    }
    let offset = (pos as i64 - origin as i64).clamp(0, size as i64 - 1);
    let n = (offset * 65536 + size as i64 - 1) / size as i64;
    n.clamp(0, 65535) as i32
}

impl Backend for SendInputBackend {
    fn move_to(&mut self, x: i32, y: i32) -> io::Result<()> {
        // SAFETY: GetSystemMetrics has no preconditions.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        mouse(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            normalize(x, vx, vw),
            normalize(y, vy, vh),
            0,
        )?;
        // Normalised coordinates can be a pixel off on odd mixed-DPI layouts;
        // snap to the exact target if so.
        let mut pos = POINT { x: 0, y: 0 };
        // SAFETY: out-pointer to a local.
        if unsafe { GetCursorPos(&mut pos) } != 0 && (pos.x, pos.y) != (x, y) {
            // SAFETY: no preconditions.
            unsafe { SetCursorPos(x, y) };
        }
        Ok(())
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> io::Result<()> {
        let (down, up, data) = match button {
            MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
            MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
            MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
            MouseButton::Back => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1 as i32),
            MouseButton::Forward => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2 as i32),
        };
        mouse(if pressed { down } else { up }, 0, 0, data)
    }

    fn wheel(&mut self, delta: i32) -> io::Result<()> {
        mouse(MOUSEEVENTF_WHEEL, 0, 0, delta)
    }

    fn hwheel(&mut self, delta: i32) -> io::Result<()> {
        mouse(MOUSEEVENTF_HWHEEL, 0, 0, delta)
    }

    fn key(&mut self, key: KeyTarget, pressed: bool) -> io::Result<()> {
        let up = if pressed { 0 } else { KEYEVENTF_KEYUP };
        let (vk, scan, flags) = match key {
            KeyTarget::Scan { code, extended } => {
                let ext = if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
                (0, code, KEYEVENTF_SCANCODE | ext | up)
            }
            KeyTarget::Vk(vk) => (vk, 0, up),
        };
        send(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: EXTRA_INFO,
                },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::normalize;

    /// What Windows does with a normalised absolute coordinate.
    fn denormalize(n: i32, origin: i32, size: i32) -> i32 {
        origin + (n as i64 * size as i64 / 65536) as i32
    }

    #[test]
    fn normalize_round_trips_every_pixel() {
        for (origin, size) in [
            (0, 1920),
            (-2560, 4480),
            (-1080, 1920 + 1080 + 3840),
            (0, 1),
        ] {
            for pos in origin..origin + size {
                let n = normalize(pos, origin, size);
                assert!((0..=65535).contains(&n));
                assert_eq!(
                    denormalize(n, origin, size),
                    pos,
                    "origin {origin} size {size}"
                );
            }
        }
    }

    #[test]
    fn normalize_clamps() {
        assert_eq!(normalize(-10, 0, 1920), 0);
        assert_eq!(normalize(5000, 0, 1920), normalize(1919, 0, 1920));
    }
}
