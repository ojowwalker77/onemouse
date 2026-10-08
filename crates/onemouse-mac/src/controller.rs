//! Decides, for every captured input event, whether it stays on the Mac or
//! goes to the secondary, and what to send. Pure logic: the macOS event tap
//! feeds it and applies the result.

use std::collections::BTreeSet;

use onemouse_protocol::key::{self, KeyCode};
use onemouse_protocol::{Display, Message, MouseButton};

use crate::keymap;
use crate::layout::{self, Point, Rect, Side, Step};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// Cursor at `pos` (Mac global points) moved by (`dx`, `dy`) points.
    Move {
        pos: Point,
        dx: f64,
        dy: f64,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    /// In protocol scroll units (120 per notch), fractions allowed.
    Scroll {
        dx: f64,
        dy: f64,
    },
    /// Physical key, before remapping.
    Key {
        code: KeyCode,
        pressed: bool,
    },
    /// Trackpad gesture (pinch, swipe…); not forwarded.
    Gesture,
}

/// The connected secondary, as the controller sees it.
#[derive(Debug, Clone, Copy)]
pub struct Peer<'a> {
    /// Changes on every new connection.
    pub id: u64,
    pub displays: &'a [Display],
}

#[derive(Debug, Default, PartialEq)]
pub struct Output {
    /// Don't let the Mac see this event.
    pub swallow: bool,
    pub send: Vec<Message>,
    /// Hide and freeze the Mac cursor: input now goes to the secondary.
    pub went_remote: bool,
    /// Show the Mac cursor again at this point.
    pub went_local: Option<Point>,
}

#[derive(Debug)]
enum State {
    Local,
    Remote {
        pos: Point,
        /// Mac display the cursor left from; it comes back there.
        from: Rect,
        scroll: (f64, f64),
        /// Connection that got the `Enter`.
        peer: u64,
    },
}

#[derive(Debug)]
pub struct Controller {
    side: Side,
    state: State,
    /// Physical keys currently down, wherever they went.
    held_keys: BTreeSet<KeyCode>,
    /// Mouse buttons down on the Mac. No crossing mid-drag.
    held_buttons: u32,
}

impl Controller {
    pub fn new(side: Side) -> Self {
        Self {
            side,
            state: State::Local,
            held_keys: BTreeSet::new(),
            held_buttons: 0,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.state, State::Remote { .. })
    }

    /// `mac` are the Mac's displays; `peer` the secondary's, if connected.
    pub fn handle(&mut self, input: Input, mac: &[Rect], peer: Option<Peer>) -> Output {
        if let Input::Key { code, pressed } = input {
            if pressed {
                self.held_keys.insert(code);
            } else {
                self.held_keys.remove(&code);
            }
        }
        match (&self.state, peer) {
            (State::Local, _) => self.local(input, mac, peer),
            (State::Remote { peer: entered, .. }, Some(peer)) if !peer.displays.is_empty() => {
                // The secondary reconnected while the cursor was on it: the new
                // connection never saw `Enter` and would ignore everything.
                let mut send = if *entered == peer.id {
                    Vec::new()
                } else {
                    self.reenter(peer)
                };
                let mut out = self.remote(input, peer.displays);
                send.append(&mut out.send);
                out.send = send;
                out
            }
            // The secondary went away: hand the cursor back.
            (State::Remote { .. }, _) => self.go_local(0.5, false),
        }
    }

    fn local(&mut self, input: Input, mac: &[Rect], peer: Option<Peer>) -> Output {
        match input {
            Input::Button { pressed, .. } => {
                if pressed {
                    self.held_buttons += 1;
                } else {
                    self.held_buttons = self.held_buttons.saturating_sub(1);
                }
            }
            Input::Move { pos, dx, dy } if self.held_buttons == 0 => {
                if let Some(peer) = peer.filter(|p| !p.displays.is_empty())
                    && let Some((from, t)) = layout::crossing(self.side, mac, pos, dx, dy)
                {
                    return self.go_remote(from, t, peer);
                }
            }
            _ => {}
        }
        Output::default()
    }

    fn go_remote(&mut self, from: Rect, t: f64, peer: Peer) -> Output {
        let rects: Vec<_> = peer.displays.iter().map(Rect::from_display).collect();
        let Some(pos) = layout::entry_point(self.side, &rects, t) else {
            return Output::default();
        };
        self.state = State::Remote {
            pos,
            from,
            scroll: (0.0, 0.0),
            peer: peer.id,
        };
        Output {
            swallow: true,
            send: self.enter_messages(pos),
            went_remote: true,
            went_local: None,
        }
    }

    /// Repeats `Enter` to a new connection, at the same spot if it still
    /// exists on the secondary's (possibly changed) displays.
    fn reenter(&mut self, peer: Peer) -> Vec<Message> {
        let State::Remote { pos, peer: id, .. } = &mut self.state else {
            return Vec::new();
        };
        let Some(display) = layout::display_at(peer.displays, *pos) else {
            return Vec::new();
        };
        *pos = Rect::from_display(display).clamp(*pos);
        *id = peer.id;
        let pos = *pos;
        self.enter_messages(pos)
    }

    fn enter_messages(&self, pos: Point) -> Vec<Message> {
        let mut send = vec![Message::Enter {
            x: pos.x.round() as i32,
            y: pos.y.round() as i32,
        }];
        // Modifiers held while crossing (Cmd-drag, Shift-click…) carry over.
        send.extend(
            self.held_keys
                .iter()
                .filter(|code| code.is_modifier())
                .map(|&code| Message::Key {
                    code: keymap::to_secondary(code),
                    pressed: true,
                }),
        );
        send
    }

    fn remote(&mut self, input: Input, peer: &[Display]) -> Output {
        let State::Remote { pos, scroll, .. } = &mut self.state else {
            unreachable!("remote() is only called while remote");
        };
        let mut send = Vec::new();
        match input {
            Input::Move { dx, dy, .. } => {
                // Mac points → secondary pixels at the target's DPI scale, so a
                // swipe covers the same visual distance on both screens.
                let scale = layout::display_at(peer, *pos).map_or(1.0, |d| f64::from(d.scale));
                let rects: Vec<_> = peer.iter().map(Rect::from_display).collect();
                match layout::move_on_secondary(self.side, &rects, *pos, dx * scale, dy * scale) {
                    Step::Stay(next) => {
                        let moved =
                            (next.x.round(), next.y.round()) != (pos.x.round(), pos.y.round());
                        *pos = next;
                        if moved {
                            send.push(Message::MouseMove {
                                x: next.x.round() as i32,
                                y: next.y.round() as i32,
                            });
                        }
                    }
                    Step::Leave(t) => return self.go_local(t, true),
                }
            }
            Input::Button { button, pressed } => {
                send.push(Message::MouseButton { button, pressed });
            }
            Input::Scroll { dx, dy } => {
                scroll.0 += dx;
                scroll.1 += dy;
                let whole = (scroll.0.trunc(), scroll.1.trunc());
                scroll.0 -= whole.0;
                scroll.1 -= whole.1;
                if whole != (0.0, 0.0) {
                    send.push(Message::Scroll {
                        dx: whole.0 as i32,
                        dy: whole.1 as i32,
                    });
                }
            }
            Input::Key { code, pressed } => {
                if code == key::ESCAPE && pressed && self.escape_chord_held() {
                    return self.go_local(0.5, true);
                }
                send.push(Message::Key {
                    code: keymap::to_secondary(code),
                    pressed,
                });
            }
            Input::Gesture => {}
        }
        Output {
            swallow: true,
            send,
            went_remote: false,
            went_local: None,
        }
    }

    /// Ctrl+Option+Cmd+Esc always brings the cursor home.
    fn escape_chord_held(&self) -> bool {
        let held = |l, r| self.held_keys.contains(&l) || self.held_keys.contains(&r);
        held(key::LEFT_CTRL, key::RIGHT_CTRL)
            && held(key::LEFT_ALT, key::RIGHT_ALT)
            && held(key::LEFT_META, key::RIGHT_META)
    }

    fn go_local(&mut self, t: f64, tell_peer: bool) -> Output {
        let State::Remote { from, .. } = self.state else {
            return Output::default();
        };
        self.state = State::Local;
        Output {
            swallow: true,
            send: if tell_peer {
                vec![Message::Leave]
            } else {
                vec![]
            },
            went_remote: false,
            went_local: Some(layout::return_point(self.side, &from, t)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AIR: Rect = Rect::new(0.0, 0.0, 1280.0, 832.0);
    const MAC: &[Rect] = &[AIR];

    fn pc(scale: f32) -> Vec<Display> {
        vec![Display {
            id: 1,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale,
            primary: true,
        }]
    }

    fn mv(x: f64, y: f64, dx: f64, dy: f64) -> Input {
        Input::Move {
            pos: Point::new(x, y),
            dx,
            dy,
        }
    }

    fn key(code: KeyCode, pressed: bool) -> Input {
        Input::Key { code, pressed }
    }

    fn peer(displays: &[Display]) -> Option<Peer<'_>> {
        Some(Peer { id: 1, displays })
    }

    /// Pushes the cursor across the Air's right edge at mid-height.
    fn cross(c: &mut Controller, displays: &[Display]) -> Output {
        c.handle(mv(1279.0, 416.0, 4.0, 0.0), MAC, peer(displays))
    }

    #[test]
    fn stays_local_without_a_peer_or_mid_drag() {
        let mut c = Controller::new(Side::Right);
        assert_eq!(
            c.handle(mv(1279.0, 416.0, 4.0, 0.0), MAC, None),
            Output::default()
        );

        let pc = pc(1.0);
        c.handle(
            Input::Button {
                button: MouseButton::Left,
                pressed: true,
            },
            MAC,
            peer(&pc),
        );
        assert_eq!(cross(&mut c, &pc), Output::default());
        assert!(!c.is_remote());
    }

    #[test]
    fn crossing_enters_and_carries_held_modifiers() {
        let mut c = Controller::new(Side::Right);
        let pc = pc(1.0);
        c.handle(key(key::LEFT_META, true), MAC, peer(&pc));
        c.handle(key(key::A, true), MAC, peer(&pc));

        let out = cross(&mut c, &pc);
        assert!(out.swallow && out.went_remote);
        assert_eq!(
            out.send,
            [
                Message::Enter { x: 0, y: 540 },
                // Cmd arrives as Ctrl; the letter isn't replayed.
                Message::Key {
                    code: key::LEFT_CTRL,
                    pressed: true
                },
            ]
        );
    }

    #[test]
    fn remote_input_is_swallowed_scaled_and_remapped() {
        let mut c = Controller::new(Side::Right);
        let pc = pc(1.5);
        cross(&mut c, &pc);

        let out = c.handle(mv(1279.0, 416.0, 10.0, -2.0), MAC, peer(&pc));
        assert!(out.swallow);
        assert_eq!(out.send, [Message::MouseMove { x: 15, y: 537 }]);

        // Sub-pixel moves accumulate instead of being sent or lost.
        let out = c.handle(mv(1279.0, 416.0, 0.2, 0.0), MAC, peer(&pc));
        assert_eq!(out.send, []);
        let out = c.handle(mv(1279.0, 416.0, 0.2, 0.0), MAC, peer(&pc));
        assert_eq!(out.send, [Message::MouseMove { x: 16, y: 537 }]);

        let out = c.handle(key(key::LEFT_CTRL, true), MAC, peer(&pc));
        assert_eq!(
            out.send,
            [Message::Key {
                code: key::LEFT_META,
                pressed: true
            }]
        );
        let out = c.handle(
            Input::Button {
                button: MouseButton::Right,
                pressed: true,
            },
            MAC,
            peer(&pc),
        );
        assert_eq!(
            out.send,
            [Message::MouseButton {
                button: MouseButton::Right,
                pressed: true
            }]
        );
        let out = c.handle(Input::Gesture, MAC, peer(&pc));
        assert!(out.swallow && out.send.is_empty());
    }

    #[test]
    fn scroll_keeps_fractions() {
        let mut c = Controller::new(Side::Right);
        let pc = pc(1.0);
        cross(&mut c, &pc);
        let scroll =
            |c: &mut Controller, dy| c.handle(Input::Scroll { dx: 0.0, dy }, MAC, peer(&pc)).send;
        assert_eq!(scroll(&mut c, 0.6), []);
        assert_eq!(scroll(&mut c, 0.6), [Message::Scroll { dx: 0, dy: 1 }]);
        assert_eq!(
            scroll(&mut c, -240.0),
            [Message::Scroll { dx: 0, dy: -239 }]
        );
    }

    #[test]
    fn moving_back_across_the_edge_leaves() {
        let mut c = Controller::new(Side::Right);
        let pc = pc(1.0);
        cross(&mut c, &pc);
        let out = c.handle(mv(1279.0, 416.0, -3.0, 0.0), MAC, peer(&pc));
        assert_eq!(
            out,
            Output {
                swallow: true,
                send: vec![Message::Leave],
                went_remote: false,
                went_local: Some(Point::new(1278.0, 416.0)),
            }
        );
        assert!(!c.is_remote());
        // Back home, input passes through untouched.
        assert_eq!(
            c.handle(key(key::A, true), MAC, peer(&pc)),
            Output::default()
        );
    }

    #[test]
    fn escape_chord_comes_home() {
        let mut c = Controller::new(Side::Right);
        let pc = pc(1.0);
        cross(&mut c, &pc);
        for code in [key::RIGHT_CTRL, key::LEFT_ALT, key::LEFT_META] {
            c.handle(key(code, true), MAC, peer(&pc));
        }
        let out = c.handle(key(key::ESCAPE, true), MAC, peer(&pc));
        assert_eq!(out.send, [Message::Leave]);
        assert!(out.went_local.is_some() && !c.is_remote());
    }

    #[test]
    fn losing_the_peer_brings_the_cursor_back() {
        let mut c = Controller::new(Side::Right);
        let pc = pc(1.0);
        cross(&mut c, &pc);
        let out = c.handle(key(key::A, true), MAC, None);
        assert_eq!(out.send, []);
        assert_eq!(out.went_local, Some(Point::new(1278.0, 416.0)));
        assert!(!c.is_remote());
    }

    #[test]
    fn reconnecting_while_remote_reenters_the_new_connection() {
        let mut c = Controller::new(Side::Right);
        let pc = pc(1.0);
        c.handle(key(key::LEFT_SHIFT, true), MAC, peer(&pc));
        cross(&mut c, &pc);
        c.handle(mv(1279.0, 416.0, 100.0, 0.0), MAC, peer(&pc));

        // Same PC, new connection, display layout changed meanwhile.
        let smaller = vec![Display {
            width: 50,
            ..pc[0].clone()
        }];
        let out = c.handle(
            key(key::A, true),
            MAC,
            Some(Peer {
                id: 2,
                displays: &smaller,
            }),
        );
        assert!(out.swallow && c.is_remote());
        assert_eq!(
            out.send,
            [
                Message::Enter { x: 49, y: 540 },
                Message::Key {
                    code: key::LEFT_SHIFT,
                    pressed: true
                },
                Message::Key {
                    code: key::A,
                    pressed: true
                },
            ]
        );
        // Only once.
        let out = c.handle(
            key(key::A, false),
            MAC,
            Some(Peer {
                id: 2,
                displays: &smaller,
            }),
        );
        assert_eq!(
            out.send,
            [Message::Key {
                code: key::A,
                pressed: false
            }]
        );
    }
}
