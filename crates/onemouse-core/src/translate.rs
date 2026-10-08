//! Shortcut translation between operating systems, run on the main machine:
//! physical keys come in, the key events the other OS expects go out.
//!
//! Two layers, both driven by the same tables in either direction:
//! - **Modifiers**: Mac Cmd ↔ PC Ctrl, Mac Ctrl ↔ PC Windows key, Option ↔
//!   Alt, Shift ↔ Shift (left/right kept).
//! - **Chords** ([`CHORDS`]): shortcuts where swapping modifiers isn't
//!   enough, e.g. Cmd+Tab ↔ Alt+Tab, Cmd+← ↔ Home, Option+← ↔ Ctrl+←.
//!
//! A modifier that lands on the PC's Windows key is held back until another
//! key needs it, so tapping Ctrl alone doesn't open Start, and Ctrl-click
//! becomes a right click as on the Mac.

use std::collections::BTreeSet;

use onemouse_protocol::key::*;
use onemouse_protocol::{KeyCode, MouseButton, Os};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Mod {
    Ctrl,
    Shift,
    Alt,
    Meta,
}

impl Mod {
    fn of(key: KeyCode) -> Option<(Self, bool)> {
        Some(match key {
            LEFT_CTRL => (Self::Ctrl, false),
            LEFT_SHIFT => (Self::Shift, false),
            LEFT_ALT => (Self::Alt, false),
            LEFT_META => (Self::Meta, false),
            RIGHT_CTRL => (Self::Ctrl, true),
            RIGHT_SHIFT => (Self::Shift, true),
            RIGHT_ALT => (Self::Alt, true),
            RIGHT_META => (Self::Meta, true),
            _ => return None,
        })
    }

    fn key(self, right: bool) -> KeyCode {
        match (self, right) {
            (Self::Ctrl, false) => LEFT_CTRL,
            (Self::Shift, false) => LEFT_SHIFT,
            (Self::Alt, false) => LEFT_ALT,
            (Self::Meta, false) => LEFT_META,
            (Self::Ctrl, true) => RIGHT_CTRL,
            (Self::Shift, true) => RIGHT_SHIFT,
            (Self::Alt, true) => RIGHT_ALT,
            (Self::Meta, true) => RIGHT_META,
        }
    }
}

/// Modifiers plus a key, as pressed on one OS.
#[derive(Debug, Clone, Copy)]
pub struct Chord {
    mods: &'static [Mod],
    key: KeyCode,
}

const fn chord(mods: &'static [Mod], key: KeyCode) -> Chord {
    Chord { mods, key }
}

use Mod::{Alt, Ctrl, Meta, Shift};

/// The same action on a Mac and on a PC.
#[derive(Debug, Clone, Copy)]
pub struct Pair {
    mac: Chord,
    pc: Chord,
    /// Keeps the translated modifiers down while the source modifiers stay
    /// down, even between key presses (the app switcher).
    sticky: bool,
}

const fn pair(mac: Chord, pc: Chord) -> Pair {
    Pair {
        mac,
        pc,
        sticky: false,
    }
}

/// Shortcuts that need more than the modifier swap. Shift held on top of
/// one passes through (Cmd+Shift+← selects to the line start).
pub const CHORDS: &[Pair] = &[
    Pair {
        mac: chord(&[Meta], TAB),
        pc: chord(&[Alt], TAB),
        sticky: true,
    },
    pair(chord(&[Meta], Q), chord(&[Alt], F4)),
    // A lone Windows-key tap opens Start, like Spotlight.
    pair(chord(&[Meta], SPACE), chord(&[], LEFT_META)),
    pair(chord(&[Meta], ARROW_LEFT), chord(&[], HOME)),
    pair(chord(&[Meta], ARROW_RIGHT), chord(&[], END)),
    pair(chord(&[Meta], ARROW_UP), chord(&[Ctrl], HOME)),
    pair(chord(&[Meta], ARROW_DOWN), chord(&[Ctrl], END)),
    pair(chord(&[Alt], ARROW_LEFT), chord(&[Ctrl], ARROW_LEFT)),
    pair(chord(&[Alt], ARROW_RIGHT), chord(&[Ctrl], ARROW_RIGHT)),
    pair(chord(&[Alt], BACKSPACE), chord(&[Ctrl], BACKSPACE)),
    pair(chord(&[Alt], DELETE), chord(&[Ctrl], DELETE)),
    pair(chord(&[Meta], M), chord(&[Meta], ARROW_DOWN)),
    pair(chord(&[Ctrl, Meta], Q), chord(&[Meta], L)),
    pair(chord(&[Meta, Shift], DIGIT_4), chord(&[Meta, Shift], S)),
    pair(chord(&[Meta, Shift], DIGIT_3), chord(&[], PRINT_SCREEN)),
    pair(chord(&[Meta, Alt], ESCAPE), chord(&[Ctrl, Shift], ESCAPE)),
];

/// One direction of translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    MacToPc,
    PcToMac,
    Same,
}

impl Direction {
    fn new(from: Os, to: Os) -> Self {
        match (from, to) {
            (Os::MacOs, Os::Windows | Os::Linux) => Self::MacToPc,
            (Os::Windows | Os::Linux, Os::MacOs) => Self::PcToMac,
            _ => Self::Same,
        }
    }

    fn modifier(self, m: Mod) -> Mod {
        match (self, m) {
            (Self::MacToPc | Self::PcToMac, Meta) => Ctrl,
            (Self::MacToPc | Self::PcToMac, Ctrl) => Meta,
            (_, m) => m,
        }
    }

    /// (source, target) for each pair.
    fn sides(self, p: &Pair) -> Option<(Chord, Chord)> {
        match self {
            Self::MacToPc => Some((p.mac, p.pc)),
            Self::PcToMac => Some((p.pc, p.mac)),
            Self::Same => None,
        }
    }

    /// Whether a source modifier is held back until another key needs it:
    /// when it would become the PC's Windows key, or a tap of it alone
    /// means something.
    fn deferred(self, m: Mod) -> bool {
        match self {
            Self::MacToPc => self.modifier(m) == Meta,
            Self::PcToMac => m == Meta,
            Self::Same => false,
        }
    }
}

/// A chord being held (or a sticky one whose modifiers are still down).
#[derive(Debug, Clone, Copy)]
struct Active {
    source_key: KeyCode,
    source_mods: &'static [Mod],
    target: Chord,
    sticky: bool,
    /// The source key is still down.
    key_down: bool,
}

/// Per-session translation state. Feed it every physical key and button
/// event while the cursor is on the other machine; send what it returns.
#[derive(Debug)]
pub struct Translator {
    direction: Direction,
    /// Whether [`CHORDS`] apply (the modifier swap always does).
    pub chords: bool,
    held: BTreeSet<KeyCode>,
    /// Target keys we pressed and haven't released.
    sent: BTreeSet<KeyCode>,
    /// Deferred modifiers not used by any key yet.
    pending: BTreeSet<KeyCode>,
    active: Vec<Active>,
    /// Ctrl-click turned into a right click; its release must match.
    right_click: bool,
}

pub type KeyEvent = (KeyCode, bool);

impl Translator {
    pub fn new(from: Os, to: Os) -> Self {
        Self {
            direction: Direction::new(from, to),
            chords: true,
            held: BTreeSet::new(),
            sent: BTreeSet::new(),
            pending: BTreeSet::new(),
            active: Vec::new(),
            right_click: false,
        }
    }

    /// Forgets everything: the other side just released all keys (`Enter` /
    /// `Leave`).
    pub fn reset(&mut self) {
        let (direction, chords) = (self.direction, self.chords);
        *self = Self::new(Os::MacOs, Os::MacOs);
        self.direction = direction;
        self.chords = chords;
    }

    /// A physical key on the main machine went down (again, for autorepeat)
    /// or up. Returns the key events to send, in order.
    pub fn key(&mut self, code: KeyCode, pressed: bool) -> Vec<KeyEvent> {
        let mut out = Vec::new();
        if let Some((m, _)) = Mod::of(code) {
            if pressed {
                let new = self.held.insert(code);
                if new && self.direction.deferred(m) {
                    self.pending.insert(code);
                }
            } else {
                self.held.remove(&code);
                if self.pending.remove(&code) {
                    self.tap(code, &mut out);
                }
                let held = self.held_mods();
                self.active
                    .retain(|a| a.key_down || a.source_mods.iter().all(|m| held.contains(m)));
            }
            self.sync(&mut out);
            return out;
        }

        if pressed {
            self.held.insert(code);
            // Held-back modifiers now count.
            self.pending.clear();
            let target = match self.matching(code) {
                Some((source, target, sticky)) => {
                    self.active
                        .retain(|a| !(a.sticky && a.source_mods == source.mods));
                    self.active.push(Active {
                        source_key: code,
                        source_mods: source.mods,
                        target,
                        sticky,
                        key_down: true,
                    });
                    target.key
                }
                None => code,
            };
            self.sync(&mut out);
            self.send(target, true, &mut out);
        } else {
            self.held.remove(&code);
            let target = match self
                .active
                .iter_mut()
                .find(|a| a.source_key == code && a.key_down)
            {
                Some(a) => {
                    a.key_down = false;
                    a.target.key
                }
                None => code,
            };
            self.send(target, false, &mut out);
            let held = self.held_mods();
            self.active.retain(|a| {
                a.key_down || (a.sticky && a.source_mods.iter().all(|m| held.contains(m)))
            });
            self.sync(&mut out);
        }
        out
    }

    /// A mouse button on the main machine. Ctrl-click (Ctrl alone) on a Mac
    /// keyboard is a right click on the PC.
    pub fn button(&mut self, button: MouseButton, pressed: bool) -> MouseButton {
        if button != MouseButton::Left || self.direction != Direction::MacToPc {
            return button;
        }
        if pressed {
            let ctrl_alone = self.held_mods() == BTreeSet::from([Ctrl])
                && self.held.iter().all(|k| Mod::of(*k).is_some());
            if ctrl_alone {
                // Ctrl was only for the click: never press the Windows key.
                self.pending.clear();
                self.right_click = true;
            }
        }
        let out = if self.right_click {
            MouseButton::Right
        } else {
            button
        };
        if !pressed {
            self.right_click = false;
        }
        out
    }

    fn held_mods(&self) -> BTreeSet<Mod> {
        self.held
            .iter()
            .filter_map(|k| Mod::of(*k).map(|(m, _)| m))
            .collect()
    }

    /// The most specific chord for `key` with what's held: its modifiers
    /// must all be held, and anything else held may only be Shift.
    fn matching(&self, key: KeyCode) -> Option<(Chord, Chord, bool)> {
        if !self.chords {
            return None;
        }
        let held = self.held_mods();
        CHORDS
            .iter()
            .filter_map(|p| Some((self.direction.sides(p)?, p.sticky)))
            .filter(|((source, _), _)| source.key == key && !source.mods.is_empty())
            .filter(|((source, _), _)| {
                source.mods.iter().all(|m| held.contains(m))
                    && held.iter().all(|m| source.mods.contains(m) || *m == Shift)
            })
            .max_by_key(|((source, _), _)| source.mods.len())
            .map(|((source, target), sticky)| (source, target, sticky))
    }

    /// A deferred modifier went up without being used: a chord whose source
    /// is that modifier alone (PC Windows key → Cmd+Space) fires now.
    fn tap(&mut self, code: KeyCode, out: &mut Vec<KeyEvent>) {
        if !self.chords {
            return;
        }
        let tapped = CHORDS
            .iter()
            .filter_map(|p| self.direction.sides(p))
            .find(|(source, _)| source.mods.is_empty() && Mod::of(source.key) == Mod::of(code));
        if let Some((_, target)) = tapped {
            let mods: Vec<_> = target.mods.iter().map(|m| m.key(false)).collect();
            for &m in &mods {
                self.send(m, true, out);
            }
            self.send(target.key, true, out);
            self.send(target.key, false, out);
            for &m in mods.iter().rev() {
                self.send(m, false, out);
            }
        }
    }

    /// Brings the target's modifiers in line with what's held and active.
    fn sync(&mut self, out: &mut Vec<KeyEvent>) {
        let mut desired = BTreeSet::new();
        let consumed: BTreeSet<Mod> = self
            .active
            .iter()
            .flat_map(|a| a.source_mods.iter().copied())
            .collect();
        for &k in &self.held {
            let Some((m, right)) = Mod::of(k) else {
                continue;
            };
            if self.pending.contains(&k) || consumed.contains(&m) {
                continue;
            }
            desired.insert(self.direction.modifier(m).key(right));
        }
        for a in &self.active {
            desired.extend(a.target.mods.iter().map(|m| m.key(false)));
        }
        let stale: Vec<_> = self
            .sent
            .iter()
            .filter(|k| Mod::of(**k).is_some() && !desired.contains(k))
            // A chord's own target key (the Windows-key tap) isn't a modifier
            // to sync here.
            .filter(|k| {
                !self
                    .active
                    .iter()
                    .any(|a| a.key_down && a.target.key == **k)
            })
            .copied()
            .collect();
        for k in stale {
            self.send(k, false, out);
        }
        for k in desired {
            if !self.sent.contains(&k) {
                self.send(k, true, out);
            }
        }
    }

    fn send(&mut self, key: KeyCode, pressed: bool, out: &mut Vec<KeyEvent>) {
        if pressed {
            self.sent.insert(key);
            out.push((key, true));
        } else if self.sent.remove(&key) {
            out.push((key, false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac_to_pc() -> Translator {
        Translator::new(Os::MacOs, Os::Windows)
    }

    fn pc_to_mac() -> Translator {
        Translator::new(Os::Windows, Os::MacOs)
    }

    /// Feeds `(key, pressed)` events and collects everything sent.
    fn run(t: &mut Translator, events: &[(KeyCode, bool)]) -> Vec<KeyEvent> {
        events.iter().flat_map(|&(k, p)| t.key(k, p)).collect()
    }

    fn down(k: KeyCode) -> (KeyCode, bool) {
        (k, true)
    }

    fn up(k: KeyCode) -> (KeyCode, bool) {
        (k, false)
    }

    #[test]
    fn cmd_c_is_ctrl_c() {
        let mut t = mac_to_pc();
        let sent = run(&mut t, &[down(LEFT_META), down(C), up(C), up(LEFT_META)]);
        assert_eq!(sent, [down(LEFT_CTRL), down(C), up(C), up(LEFT_CTRL)]);
    }

    #[test]
    fn plain_typing_and_shift_pass_through_with_repeat() {
        let mut t = mac_to_pc();
        let sent = run(
            &mut t,
            &[down(LEFT_SHIFT), down(A), down(A), up(A), up(LEFT_SHIFT)],
        );
        assert_eq!(
            sent,
            [down(LEFT_SHIFT), down(A), down(A), up(A), up(LEFT_SHIFT)]
        );
    }

    #[test]
    fn a_lone_ctrl_tap_never_reaches_the_pc() {
        let mut t = mac_to_pc();
        assert_eq!(run(&mut t, &[down(LEFT_CTRL), up(LEFT_CTRL)]), []);
    }

    #[test]
    fn ctrl_with_a_key_becomes_the_windows_key() {
        let mut t = mac_to_pc();
        let sent = run(
            &mut t,
            &[down(LEFT_CTRL), down(ARROW_UP), up(ARROW_UP), up(LEFT_CTRL)],
        );
        assert_eq!(
            sent,
            [down(LEFT_META), down(ARROW_UP), up(ARROW_UP), up(LEFT_META)]
        );
    }

    #[test]
    fn ctrl_click_is_a_right_click_without_the_windows_key() {
        let mut t = mac_to_pc();
        assert_eq!(t.key(LEFT_CTRL, true), []);
        assert_eq!(t.button(MouseButton::Left, true), MouseButton::Right);
        assert_eq!(t.button(MouseButton::Left, false), MouseButton::Right);
        assert_eq!(t.key(LEFT_CTRL, false), []);
        // A plain click afterwards is a plain click.
        assert_eq!(t.button(MouseButton::Left, true), MouseButton::Left);
        assert_eq!(t.button(MouseButton::Left, false), MouseButton::Left);
    }

    #[test]
    fn cmd_tab_keeps_alt_down_for_the_switcher() {
        let mut t = mac_to_pc();
        let sent = run(
            &mut t,
            &[
                down(LEFT_META),
                down(TAB),
                up(TAB),
                down(TAB),
                up(TAB),
                up(LEFT_META),
            ],
        );
        assert_eq!(
            sent,
            [
                down(LEFT_CTRL),
                // Ctrl swapped for Alt as Tab goes down…
                up(LEFT_CTRL),
                down(LEFT_ALT),
                down(TAB),
                up(TAB),
                // …and Alt stays down between Tabs.
                down(TAB),
                up(TAB),
                up(LEFT_ALT),
            ]
        );
    }

    #[test]
    fn cmd_shift_tab_goes_backwards() {
        let mut t = mac_to_pc();
        let sent = run(
            &mut t,
            &[down(LEFT_META), down(LEFT_SHIFT), down(TAB), up(TAB)],
        );
        assert_eq!(
            sent,
            [
                down(LEFT_CTRL),
                down(LEFT_SHIFT),
                up(LEFT_CTRL),
                down(LEFT_ALT),
                down(TAB),
                up(TAB),
            ]
        );
    }

    #[test]
    fn cmd_q_is_alt_f4_then_cmd_is_ctrl_again() {
        let mut t = mac_to_pc();
        let sent = run(
            &mut t,
            &[
                down(LEFT_META),
                down(Q),
                up(Q),
                down(W),
                up(W),
                up(LEFT_META),
            ],
        );
        assert_eq!(
            sent,
            [
                down(LEFT_CTRL),
                up(LEFT_CTRL),
                down(LEFT_ALT),
                down(F4),
                up(F4),
                up(LEFT_ALT),
                down(LEFT_CTRL),
                down(W),
                up(W),
                up(LEFT_CTRL),
            ]
        );
    }

    #[test]
    fn cmd_shift_arrow_selects_to_line_start() {
        let mut t = mac_to_pc();
        let sent = run(
            &mut t,
            &[
                down(LEFT_META),
                down(LEFT_SHIFT),
                down(ARROW_LEFT),
                up(ARROW_LEFT),
                up(LEFT_SHIFT),
                up(LEFT_META),
            ],
        );
        assert_eq!(
            sent,
            [
                down(LEFT_CTRL),
                down(LEFT_SHIFT),
                up(LEFT_CTRL),
                down(HOME),
                up(HOME),
                down(LEFT_CTRL),
                up(LEFT_SHIFT),
                up(LEFT_CTRL),
            ]
        );
    }

    #[test]
    fn option_arrow_jumps_words() {
        let mut t = mac_to_pc();
        let sent = run(
            &mut t,
            &[
                down(LEFT_ALT),
                down(ARROW_RIGHT),
                up(ARROW_RIGHT),
                up(LEFT_ALT),
            ],
        );
        assert_eq!(
            sent,
            [
                down(LEFT_ALT),
                up(LEFT_ALT),
                down(LEFT_CTRL),
                down(ARROW_RIGHT),
                up(ARROW_RIGHT),
                up(LEFT_CTRL),
                down(LEFT_ALT),
                up(LEFT_ALT),
            ]
        );
    }

    #[test]
    fn cmd_space_taps_the_windows_key() {
        let mut t = mac_to_pc();
        let sent = run(
            &mut t,
            &[down(LEFT_META), down(SPACE), up(SPACE), up(LEFT_META)],
        );
        assert_eq!(
            sent,
            [
                down(LEFT_CTRL),
                up(LEFT_CTRL),
                down(LEFT_META),
                up(LEFT_META),
                // The chord ended with Cmd still held: plain Cmd→Ctrl
                // comes back until Cmd goes up.
                down(LEFT_CTRL),
                up(LEFT_CTRL),
            ]
        );
    }

    #[test]
    fn more_specific_chords_win() {
        let mut t = mac_to_pc();
        let sent = run(
            &mut t,
            &[
                down(LEFT_CTRL),
                down(LEFT_META),
                down(Q),
                up(Q),
                up(LEFT_META),
                up(LEFT_CTRL),
            ],
        );
        assert_eq!(
            sent,
            [
                down(LEFT_CTRL),
                up(LEFT_CTRL),
                down(LEFT_META),
                down(L),
                up(L),
                // Back to the plain swap while both are still held…
                down(LEFT_CTRL),
                // …then both release.
                up(LEFT_CTRL),
                up(LEFT_META),
            ]
        );
    }

    #[test]
    fn pc_to_mac_runs_the_same_table_backwards() {
        let mut t = pc_to_mac();
        // Ctrl+C → Cmd+C.
        assert_eq!(
            run(&mut t, &[down(LEFT_CTRL), down(C), up(C), up(LEFT_CTRL)]),
            [down(LEFT_META), down(C), up(C), up(LEFT_META)]
        );
        // Alt+Tab → Cmd+Tab, Cmd kept between Tabs.
        assert_eq!(
            run(
                &mut t,
                &[
                    down(LEFT_ALT),
                    down(TAB),
                    up(TAB),
                    down(TAB),
                    up(TAB),
                    up(LEFT_ALT)
                ]
            ),
            [
                down(LEFT_ALT),
                up(LEFT_ALT),
                down(LEFT_META),
                down(TAB),
                up(TAB),
                down(TAB),
                up(TAB),
                up(LEFT_META),
            ]
        );
        // Home → Cmd+←.
        assert_eq!(
            run(&mut t, &[down(HOME), up(HOME)]),
            // Home has no modifiers on the PC side, so it isn't a chord
            // source: the table only fires on chords with modifiers.
            [down(HOME), up(HOME)]
        );
        // Ctrl+← → Option+←.
        assert_eq!(
            run(
                &mut t,
                &[
                    down(LEFT_CTRL),
                    down(ARROW_LEFT),
                    up(ARROW_LEFT),
                    up(LEFT_CTRL)
                ]
            ),
            [
                down(LEFT_META),
                up(LEFT_META),
                down(LEFT_ALT),
                down(ARROW_LEFT),
                up(ARROW_LEFT),
                up(LEFT_ALT),
                // The chord ended with Ctrl still held: plain Ctrl→Cmd
                // comes back until Ctrl goes up.
                down(LEFT_META),
                up(LEFT_META),
            ]
        );
    }

    #[test]
    fn a_lone_windows_key_tap_is_cmd_space() {
        let mut t = pc_to_mac();
        assert_eq!(
            run(&mut t, &[down(LEFT_META), up(LEFT_META)]),
            [down(LEFT_META), down(SPACE), up(SPACE), up(LEFT_META)]
        );
        // Windows key with something else is the Mac's Ctrl.
        assert_eq!(
            run(&mut t, &[down(LEFT_META), down(A), up(A), up(LEFT_META)]),
            [down(LEFT_CTRL), down(A), up(A), up(LEFT_CTRL)]
        );
    }

    #[test]
    fn chords_can_be_turned_off() {
        let mut t = mac_to_pc();
        t.chords = false;
        assert_eq!(
            run(
                &mut t,
                &[down(LEFT_META), down(TAB), up(TAB), up(LEFT_META)]
            ),
            [down(LEFT_CTRL), down(TAB), up(TAB), up(LEFT_CTRL)]
        );
    }

    #[test]
    fn same_os_is_untouched() {
        let mut t = Translator::new(Os::MacOs, Os::MacOs);
        assert_eq!(
            run(&mut t, &[down(LEFT_CTRL), down(LEFT_META), down(Q), up(Q)]),
            [down(LEFT_CTRL), down(LEFT_META), down(Q), up(Q)]
        );
    }

    #[test]
    fn reset_forgets_held_keys() {
        let mut t = mac_to_pc();
        run(&mut t, &[down(LEFT_META), down(TAB)]);
        t.reset();
        assert_eq!(run(&mut t, &[down(A), up(A)]), [down(A), up(A)]);
    }

    #[test]
    fn every_chord_side_uses_left_modifiers_and_distinct_sources() {
        for direction in [Direction::MacToPc, Direction::PcToMac] {
            let sources: Vec<_> = CHORDS
                .iter()
                .filter_map(|p| direction.sides(p))
                .map(|(s, _)| (s.key, s.mods.to_vec()))
                .collect();
            for (i, s) in sources.iter().enumerate() {
                assert!(
                    !sources[..i].contains(s),
                    "{direction:?}: duplicate source {s:?}"
                );
            }
        }
    }
}
