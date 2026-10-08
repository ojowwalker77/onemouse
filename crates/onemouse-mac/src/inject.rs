//! The Mac as the remote side (the PC is main): turns protocol input into
//! macOS events. The state machine is portable and tested on every runner;
//! posting the events is a [`Backend`] (CoreGraphics in `macos`).
//!
//! macOS needs more than the bare input: drags are their own event type,
//! double clicks carry a click count, and every key event carries the
//! modifier flags held at the time, so all of that is tracked here.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use onemouse_protocol::key::{self, KeyCode};
use onemouse_protocol::{Message, MouseButton};

use crate::keymap;

/// `CGEventFlags` for each modifier: the generic mask apps check, plus the
/// left/right device bit.
const SHIFT: u64 = 0x0002_0000;
const CONTROL: u64 = 0x0004_0000;
const ALTERNATE: u64 = 0x0008_0000;
const COMMAND: u64 = 0x0010_0000;

/// Protocol scroll units per pixel; one notch scrolls 60 px, like the
/// Mac→PC direction in reverse.
const SCROLL_UNITS_PER_PIXEL: f64 = 2.0;
/// Clicks closer than this, in time and points, count as a double click.
const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_DISTANCE: f64 = 4.0;

/// One mouse event for the backend to post.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mouse {
    Moved,
    /// A move while `button` is held.
    Dragged(MouseButton),
    Down(MouseButton),
    Up(MouseButton),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Down,
    Up,
    /// A modifier went down or up; the new state is in the flags.
    FlagsChanged,
}

/// Posts events. Positions are global display points.
pub trait Backend {
    fn mouse(&mut self, event: Mouse, at: (f64, f64), delta: (f64, f64), clicks: i64, flags: u64);
    fn key(&mut self, vk: u16, event: KeyEvent, flags: u64);
    /// Pixels; `dy > 0` scrolls up, `dx > 0` right (protocol directions).
    fn scroll(&mut self, dx: i32, dy: i32, flags: u64);
}

#[derive(Debug)]
pub struct Injector<B> {
    backend: B,
    iso: bool,
    pos: (f64, f64),
    /// Keys we pressed and haven't released, modifiers included.
    keys: BTreeSet<KeyCode>,
    buttons: Vec<MouseButton>,
    /// Last press, for double clicks: button, where, when, count.
    last_click: Option<(MouseButton, (f64, f64), Instant, i64)>,
    /// Sub-pixel scroll carried to the next event.
    scroll: (f64, f64),
}

impl<B: Backend> Injector<B> {
    pub fn new(backend: B, iso: bool) -> Self {
        Self {
            backend,
            iso,
            pos: (0.0, 0.0),
            keys: BTreeSet::new(),
            buttons: Vec::new(),
            last_click: None,
            scroll: (0.0, 0.0),
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Applies one input message from the main side; others are ignored.
    pub fn handle(&mut self, msg: &Message, now: Instant) {
        match *msg {
            Message::Enter { x, y } => {
                // The main side assumes nothing is held from before.
                self.release_all();
                self.pos = (f64::from(x), f64::from(y));
                self.move_to(self.pos.0, self.pos.1);
            }
            Message::Leave => self.release_all(),
            Message::MouseMove { x, y } => self.move_to(f64::from(x), f64::from(y)),
            Message::MouseButton { button, pressed } => self.button(button, pressed, now),
            Message::Scroll { dx, dy } => self.scroll(dx, dy),
            Message::Key { code, pressed } => self.key(code, pressed),
            _ => {}
        }
    }

    /// Lets go of every key and button we still hold (Leave, disconnect,
    /// the Mac becoming main).
    pub fn release_all(&mut self) {
        // Plain keys first, so they don't turn into shortcuts on the way up.
        let (modifiers, plain): (Vec<_>, Vec<_>) =
            self.keys.iter().partition(|&&k| modifier(k).is_some());
        for code in plain.into_iter().chain(modifiers) {
            self.key(code, false);
        }
        for button in std::mem::take(&mut self.buttons) {
            self.backend
                .mouse(Mouse::Up(button), self.pos, (0.0, 0.0), 1, self.flags());
        }
        self.last_click = None;
        self.scroll = (0.0, 0.0);
    }

    fn flags(&self) -> u64 {
        self.keys
            .iter()
            .filter_map(|&k| modifier(k))
            .fold(0, |a, f| a | f)
    }

    fn move_to(&mut self, x: f64, y: f64) {
        let delta = (x - self.pos.0, y - self.pos.1);
        self.pos = (x, y);
        let event = match self.buttons.first() {
            Some(&b) => Mouse::Dragged(b),
            None => Mouse::Moved,
        };
        self.backend.mouse(event, self.pos, delta, 0, self.flags());
    }

    fn button(&mut self, button: MouseButton, pressed: bool, now: Instant) {
        let held = self.buttons.contains(&button);
        if pressed == held {
            return;
        }
        let clicks = if pressed {
            let clicks = match self.last_click {
                Some((b, (x, y), at, n))
                    if b == button
                        && now.saturating_duration_since(at) <= DOUBLE_CLICK_TIME
                        && (x - self.pos.0).hypot(y - self.pos.1) <= DOUBLE_CLICK_DISTANCE =>
                {
                    n + 1
                }
                _ => 1,
            };
            self.last_click = Some((button, self.pos, now, clicks));
            self.buttons.push(button);
            clicks
        } else {
            self.buttons.retain(|&b| b != button);
            self.last_click.filter(|c| c.0 == button).map_or(1, |c| c.3)
        };
        let event = if pressed {
            Mouse::Down(button)
        } else {
            Mouse::Up(button)
        };
        self.backend
            .mouse(event, self.pos, (0.0, 0.0), clicks, self.flags());
    }

    fn scroll(&mut self, dx: i32, dy: i32) {
        self.scroll.0 += f64::from(dx) / SCROLL_UNITS_PER_PIXEL;
        self.scroll.1 += f64::from(dy) / SCROLL_UNITS_PER_PIXEL;
        let (x, y) = (self.scroll.0.trunc(), self.scroll.1.trunc());
        self.scroll.0 -= x;
        self.scroll.1 -= y;
        if x != 0.0 || y != 0.0 {
            self.backend.scroll(x as i32, y as i32, self.flags());
        }
    }

    fn key(&mut self, code: KeyCode, pressed: bool) {
        if pressed == self.keys.contains(&code) {
            return; // auto-repeat isn't sent; a repeated press changes nothing
        }
        if pressed {
            self.keys.insert(code);
        } else {
            self.keys.remove(&code);
        }
        let Some(vk) = keymap::to_mac(code, self.iso) else {
            return;
        };
        let event = match (modifier(code), pressed) {
            (Some(_), _) => KeyEvent::FlagsChanged,
            (None, true) => KeyEvent::Down,
            (None, false) => KeyEvent::Up,
        };
        self.backend.key(vk, event, self.flags());
    }
}

/// The flags a held modifier contributes, `None` for other keys.
fn modifier(code: KeyCode) -> Option<u64> {
    Some(match code {
        key::LEFT_CTRL => CONTROL | 0x0001,
        key::RIGHT_CTRL => CONTROL | 0x2000,
        key::LEFT_SHIFT => SHIFT | 0x0002,
        key::RIGHT_SHIFT => SHIFT | 0x0004,
        key::LEFT_META => COMMAND | 0x0008,
        key::RIGHT_META => COMMAND | 0x0010,
        key::LEFT_ALT => ALTERNATE | 0x0020,
        key::RIGHT_ALT => ALTERNATE | 0x0040,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use onemouse_protocol::SCROLL_UNITS_PER_NOTCH;

    #[derive(Debug, Clone, PartialEq)]
    enum Posted {
        Mouse(Mouse, (f64, f64), (f64, f64), i64, u64),
        Key(u16, KeyEvent, u64),
        Scroll(i32, i32),
    }

    #[derive(Debug, Default)]
    struct Record(Vec<Posted>);

    impl Backend for Record {
        fn mouse(&mut self, e: Mouse, at: (f64, f64), d: (f64, f64), clicks: i64, flags: u64) {
            self.0.push(Posted::Mouse(e, at, d, clicks, flags));
        }
        fn key(&mut self, vk: u16, event: KeyEvent, flags: u64) {
            self.0.push(Posted::Key(vk, event, flags));
        }
        fn scroll(&mut self, dx: i32, dy: i32, _flags: u64) {
            self.0.push(Posted::Scroll(dx, dy));
        }
    }

    fn injector() -> Injector<Record> {
        Injector::new(Record::default(), false)
    }

    fn take(i: &mut Injector<Record>) -> Vec<Posted> {
        std::mem::take(&mut i.backend.0)
    }

    fn send(i: &mut Injector<Record>, msgs: &[Message]) -> Vec<Posted> {
        let now = Instant::now();
        for m in msgs {
            i.handle(m, now);
        }
        take(i)
    }

    const CMD: u64 = COMMAND | 0x0008;
    const VK_C: u16 = 0x08;
    const VK_CMD: u16 = 0x37;

    #[test]
    fn moves_absolutely_with_deltas_and_drags_while_held() {
        let mut i = injector();
        let left = MouseButton::Left;
        let posted = send(
            &mut i,
            &[
                Message::Enter { x: 10, y: 20 },
                Message::MouseMove { x: 13, y: 16 },
                Message::MouseButton {
                    button: left,
                    pressed: true,
                },
                Message::MouseMove { x: 15, y: 16 },
                Message::MouseButton {
                    button: left,
                    pressed: false,
                },
            ],
        );
        assert_eq!(
            posted,
            [
                Posted::Mouse(Mouse::Moved, (10.0, 20.0), (0.0, 0.0), 0, 0),
                Posted::Mouse(Mouse::Moved, (13.0, 16.0), (3.0, -4.0), 0, 0),
                Posted::Mouse(Mouse::Down(left), (13.0, 16.0), (0.0, 0.0), 1, 0),
                Posted::Mouse(Mouse::Dragged(left), (15.0, 16.0), (2.0, 0.0), 0, 0),
                Posted::Mouse(Mouse::Up(left), (15.0, 16.0), (0.0, 0.0), 1, 0),
            ]
        );
    }

    #[test]
    fn counts_double_clicks_in_time_and_place() {
        let mut i = injector();
        let t = Instant::now();
        let click = |i: &mut Injector<Record>, at: Instant| {
            for pressed in [true, false] {
                i.handle(
                    &Message::MouseButton {
                        button: MouseButton::Left,
                        pressed,
                    },
                    at,
                );
            }
            take(i)
                .into_iter()
                .map(|p| match p {
                    Posted::Mouse(_, _, _, n, _) => n,
                    other => panic!("{other:?}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(click(&mut i, t), [1, 1]);
        assert_eq!(click(&mut i, t + Duration::from_millis(200)), [2, 2]);
        assert_eq!(click(&mut i, t + Duration::from_millis(400)), [3, 3]);
        // Too late: a new single click.
        assert_eq!(click(&mut i, t + Duration::from_secs(2)), [1, 1]);
        // Moved away: single again.
        i.handle(&Message::MouseMove { x: 50, y: 50 }, t);
        take(&mut i);
        assert_eq!(click(&mut i, t + Duration::from_millis(2100)), [1, 1]);
    }

    #[test]
    fn keys_carry_held_modifier_flags() {
        let mut i = injector();
        let posted = send(
            &mut i,
            &[
                Message::Key {
                    code: key::LEFT_META,
                    pressed: true,
                },
                Message::Key {
                    code: key::C,
                    pressed: true,
                },
                Message::Key {
                    code: key::C,
                    pressed: false,
                },
                Message::Key {
                    code: key::LEFT_META,
                    pressed: false,
                },
            ],
        );
        assert_eq!(
            posted,
            [
                Posted::Key(VK_CMD, KeyEvent::FlagsChanged, CMD),
                Posted::Key(VK_C, KeyEvent::Down, CMD),
                Posted::Key(VK_C, KeyEvent::Up, CMD),
                Posted::Key(VK_CMD, KeyEvent::FlagsChanged, 0),
            ]
        );
    }

    #[test]
    fn leave_releases_plain_keys_then_modifiers_then_buttons() {
        let mut i = injector();
        send(
            &mut i,
            &[
                Message::Key {
                    code: key::LEFT_META,
                    pressed: true,
                },
                Message::Key {
                    code: key::C,
                    pressed: true,
                },
                Message::MouseButton {
                    button: MouseButton::Right,
                    pressed: true,
                },
            ],
        );
        assert_eq!(
            send(&mut i, &[Message::Leave]),
            [
                Posted::Key(VK_C, KeyEvent::Up, CMD),
                Posted::Key(VK_CMD, KeyEvent::FlagsChanged, 0),
                Posted::Mouse(Mouse::Up(MouseButton::Right), (0.0, 0.0), (0.0, 0.0), 1, 0),
            ]
        );
        // Nothing left to release.
        assert_eq!(send(&mut i, &[Message::Leave]), []);
    }

    #[test]
    fn enter_releases_leftovers_first() {
        let mut i = injector();
        send(
            &mut i,
            &[Message::Key {
                code: key::A,
                pressed: true,
            }],
        );
        let posted = send(&mut i, &[Message::Enter { x: 1, y: 1 }]);
        assert_eq!(posted[0], Posted::Key(0x00, KeyEvent::Up, 0));
        assert!(matches!(posted[1], Posted::Mouse(Mouse::Moved, ..)));
    }

    #[test]
    fn repeated_presses_and_stray_releases_are_dropped() {
        let mut i = injector();
        let posted = send(
            &mut i,
            &[
                Message::Key {
                    code: key::A,
                    pressed: false,
                },
                Message::Key {
                    code: key::A,
                    pressed: true,
                },
                Message::Key {
                    code: key::A,
                    pressed: true,
                },
                Message::MouseButton {
                    button: MouseButton::Left,
                    pressed: false,
                },
            ],
        );
        assert_eq!(posted, [Posted::Key(0x00, KeyEvent::Down, 0)]);
    }

    #[test]
    fn scroll_converts_units_and_keeps_fractions() {
        let mut i = injector();
        let posted = send(
            &mut i,
            &[
                Message::Scroll {
                    dx: 0,
                    dy: SCROLL_UNITS_PER_NOTCH,
                },
                Message::Scroll { dx: -1, dy: 0 },
                Message::Scroll { dx: -1, dy: 0 },
            ],
        );
        assert_eq!(posted, [Posted::Scroll(0, 60), Posted::Scroll(-1, 0)]);
    }
}
