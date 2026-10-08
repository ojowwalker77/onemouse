//! Windows-as-main capture: while the cursor is on the Mac, low-level hooks
//! swallow this PC's input and it goes to the Mac instead.
//!
//! [`MainSide`] is the portable Controller wiring (units, translation,
//! cursor actions) tested on every CI runner. The `WH_KEYBOARD_LL` /
//! `WH_MOUSE_LL` hooks and the cursor hide/park are `cfg(windows)`;
//! elsewhere [`start`] returns a stub, so role transitions still behave
//! (a `Leave` is sent when demoted) without ever going remote.
//!
//! # Units
//!
//! The Controller works in main-side *visual* units with the remote side's
//! scale converting to remote units (`remote = visual × scale`). The PC's
//! visual unit is the physical pixel divided by the display's DPI scale
//! (≈ a CSS px), and the Mac reports its displays in points with
//! `scale = 1.0`, so points map 1:1 onto visual units.

#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(test, windows))]
use std::sync::mpsc::Receiver;
use std::sync::mpsc::{self, Sender};
#[cfg(windows)]
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::thread::JoinHandle;

use onemouse_core::controller::{Controller, Input, Peer};
use onemouse_core::layout::{Point, Rect, Side};
use onemouse_protocol::{Display, Message, MouseButton, Os};

use crate::keymap;

/// What the main side needs to know about the server (the Mac).
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub os: Os,
    pub displays: Vec<Display>,
    /// The client's desktop origin in server units, if arranged yet.
    pub arrangement: Option<Point>,
}

impl ServerInfo {
    pub fn new(os: Os, displays: Vec<Display>) -> Self {
        Self {
            os,
            displays,
            arrangement: None,
        }
    }
}

/// What the cursor does when the input leaves or returns to this PC.
/// Positions are physical pixels.
pub trait Cursor: Send {
    /// Input now goes to the Mac: hide the cursor and pin it at `at`.
    fn went_remote(&mut self, at: (i32, i32));
    /// Back on this PC, at `at` when the controller knows where.
    fn went_local(&mut self, at: Option<(i32, i32)>);
}

fn scale_of(d: &Display) -> f64 {
    let scale = f64::from(d.scale);
    if scale > 0.0 { scale } else { 1.0 }
}

/// This PC's desktop in visual units, with the way back to physical pixels.
#[derive(Debug, Clone, Default)]
struct LocalSpace {
    /// Displays in visual units (kept in physical layout, divided).
    rects: Vec<Rect>,
    /// Physical origin and scale per rect, to invert the mapping.
    origin_px: Vec<(i32, i32)>,
    scale: Vec<f64>,
}

impl LocalSpace {
    fn new(displays: &[Display]) -> Self {
        // Like `layout::place`, but keeping the real (possibly negative)
        // positions: offsets use the primary display's scale, sizes each
        // display's own. Single display (the common case) is exact.
        let reference = displays
            .iter()
            .find(|d| d.primary)
            .or(displays.first())
            .map_or(1.0, scale_of);
        let left = displays.iter().map(|d| d.x).min().unwrap_or(0);
        let top = displays.iter().map(|d| d.y).min().unwrap_or(0);
        let mut space = Self::default();
        for d in displays {
            let scale = scale_of(d);
            space.rects.push(Rect::new(
                f64::from(d.x - left) / reference,
                f64::from(d.y - top) / reference,
                f64::from(d.width) / scale,
                f64::from(d.height) / scale,
            ));
            space.origin_px.push((d.x, d.y));
            space.scale.push(scale);
        }
        space
    }

    /// Physical pixels → visual units, inverting the `new` mapping above.
    fn to_visual(&self, x: i32, y: i32) -> Point {
        if self.rects.is_empty() {
            return Point::new(f64::from(x), f64::from(y));
        }
        // The display under the cursor; its rect inverts exactly there.
        let primary = self.scale.first().copied().unwrap_or(1.0);
        let idx = self
            .origin_px
            .iter()
            .enumerate()
            .find(|(n, _)| {
                let (ox, oy) = self.origin_px[*n];
                let r = self.rects[*n];
                r.contains(Point::new(
                    r.x + f64::from(x - ox) / primary,
                    r.y + f64::from(y - oy) / primary,
                ))
            })
            .map(|(n, _)| n)
            .unwrap_or(0);
        let r = self.rects[idx];
        let (ox, oy) = self.origin_px[idx];
        Point::new(
            r.x + f64::from(x - ox) / primary,
            r.y + f64::from(y - oy) / primary,
        )
    }

    /// Visual units → physical pixels.
    fn to_physical(&self, p: Point) -> (i32, i32) {
        let Some((idx, _)) = self.rects.iter().enumerate().find(|(_, r)| r.contains(p)) else {
            // Off every display (rounding at an edge): use the first.
            if self.rects.is_empty() {
                return (p.x.round() as i32, p.y.round() as i32);
            }
            let (ox, oy) = self.origin_px[0];
            let primary = self.scale[0];
            let r = self.rects[0];
            return (
                (ox as f64 + (p.x - r.x) * primary).round() as i32,
                (oy as f64 + (p.y - r.y) * primary).round() as i32,
            );
        };
        let (ox, oy) = self.origin_px[idx];
        let primary = self.scale.first().copied().unwrap_or(1.0);
        let r = self.rects[idx];
        (
            (ox as f64 + (p.x - r.x) * primary).round() as i32,
            (oy as f64 + (p.y - r.y) * primary).round() as i32,
        )
    }
}

/// The main side: feeds local input to the Controller and applies the
/// result (send to the server, hide/park the cursor).
pub struct MainSide<C: Cursor> {
    controller: Controller,
    local: LocalSpace,
    server: Vec<Display>,
    peer: u64,
    outgoing: Sender<Message>,
    cursor: C,
    last: Option<Point>,
    last_physical: (i32, i32),
}

impl<C: Cursor> MainSide<C> {
    pub fn new(
        outgoing: Sender<Message>,
        cursor: C,
        local: &[Display],
        server: &ServerInfo,
        peer: u64,
    ) -> Self {
        Self {
            controller: Controller::for_direction(
                Os::Windows,
                server.os,
                Side::Left,
                server.arrangement,
            ),
            local: LocalSpace::new(local),
            server: server.displays.clone(),
            peer,
            outgoing,
            cursor,
            last: None,
            last_physical: (0, 0),
        }
    }

    /// New displays/arrangement (or a new connection `peer`): no input lost.
    pub fn update(&mut self, local: &[Display], server: &ServerInfo, peer: Option<u64>) {
        self.local = LocalSpace::new(local);
        self.controller.set_arrangement(server.arrangement);
        self.server = server.displays.clone();
        if let Some(peer) = peer {
            self.peer = peer;
        }
        // Positions may mean something else now; resync on the next event.
        self.last = None;
    }

    pub fn is_remote(&self) -> bool {
        self.controller.is_remote()
    }

    /// The server as the controller sees it: separate borrows so callers
    /// can hold `controller` mutably alongside.
    fn peer_parts(server: &[Display], peer: u64) -> Option<Peer<'_>> {
        if server.is_empty() {
            None
        } else {
            Some(Peer {
                id: peer,
                displays: server,
            })
        }
    }

    fn send(&self, messages: Vec<Message>) {
        for msg in messages {
            let _ = self.outgoing.send(msg);
        }
    }

    /// Absolute cursor position in physical pixels. Returns whether the
    /// hook must swallow the event.
    pub fn mouse_at(&mut self, x: i32, y: i32) -> bool {
        let pos = self.local.to_visual(x, y);
        let (dx, dy) = match self.last {
            Some(last) => (pos.x - last.x, pos.y - last.y),
            None => (0.0, 0.0),
        };
        self.last = Some(pos);
        self.last_physical = (x, y);
        if dx == 0.0 && dy == 0.0 && !self.controller.is_remote() {
            return false;
        }
        let peer = Self::peer_parts(&self.server, self.peer);
        let out = self
            .controller
            .handle(Input::Move { pos, dx, dy }, &self.local.rects, peer);
        self.apply(out, Some((x, y)))
    }

    /// A mouse button in physical pixels.
    pub fn mouse_button(&mut self, button: MouseButton, pressed: bool, x: i32, y: i32) -> bool {
        self.sync_pos(x, y);
        let peer = Self::peer_parts(&self.server, self.peer);
        let out =
            self.controller
                .handle(Input::Button { button, pressed }, &self.local.rects, peer);
        self.apply(out, None)
    }

    /// Wheel deltas in protocol units (120 per notch, as Windows reports).
    pub fn mouse_scroll(&mut self, dx: i32, dy: i32, x: i32, y: i32) -> bool {
        self.sync_pos(x, y);
        let peer = Self::peer_parts(&self.server, self.peer);
        let out = self.controller.handle(
            Input::Scroll {
                dx: f64::from(dx),
                dy: f64::from(dy),
            },
            &self.local.rects,
            peer,
        );
        self.apply(out, None)
    }

    /// A physical key (`None` when the hook can't map it: still swallowed
    /// while remote so nothing leaks to local apps).
    pub fn key(&mut self, code: Option<onemouse_protocol::KeyCode>, pressed: bool) -> bool {
        let Some(code) = code else {
            return self.controller.is_remote();
        };
        let peer = Self::peer_parts(&self.server, self.peer);
        let out = self
            .controller
            .handle(Input::Key { code, pressed }, &self.local.rects, peer);
        self.apply(out, None)
    }

    /// The cursor moved without an event we saw (button/scroll carry a
    /// position but must never trigger a crossing on their own).
    fn sync_pos(&mut self, x: i32, y: i32) {
        self.last = Some(self.local.to_visual(x, y));
        self.last_physical = (x, y);
    }

    fn apply(&mut self, out: onemouse_core::controller::Output, at: Option<(i32, i32)>) -> bool {
        self.send(out.send);
        if out.went_remote {
            // Only a move crosses, so `at` is the event position; fall
            // back to the last known one for safety.
            self.cursor.went_remote(at.unwrap_or(self.last_physical));
        }
        if let Some(point) = out.went_local {
            let physical = self.local.to_physical(point);
            self.cursor.went_local(Some(physical));
            self.last = Some(point);
            self.last_physical = physical;
        }
        out.swallow
    }
}

/// A hook keyboard event → controller input (`None` = unmappable).
pub fn key_input(vk: u16, scan: u16, extended: bool, up: bool) -> Option<Input> {
    keymap::from_hook(vk, scan, extended).map(|code| Input::Key { code, pressed: !up })
}

/// Which mouse action a `WH_MOUSE_LL` `wParam` is. `xbutton` is
/// `HIWORD(mouseData)` for the `XBUTTON` messages (1 = back, else forward).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseAction {
    Move,
    Button { button: MouseButton, pressed: bool },
    Scroll { dx: i32, dy: i32 },
}

/// `wparam` values match the Win32 `WM_*BUTTON*` / `WM_MOUSE*` constants.
pub fn mouse_action(wparam: u32, wheel: i32, xbutton: u16) -> Option<MouseAction> {
    Some(match wparam {
        // WM_MOUSEMOVE
        0x0200 => MouseAction::Move,
        // WM_LBUTTONDOWN / WM_LBUTTONUP
        0x0201 => MouseAction::Button {
            button: MouseButton::Left,
            pressed: true,
        },
        0x0202 => MouseAction::Button {
            button: MouseButton::Left,
            pressed: false,
        },
        // WM_RBUTTONDOWN / WM_RBUTTONUP
        0x0204 => MouseAction::Button {
            button: MouseButton::Right,
            pressed: true,
        },
        0x0205 => MouseAction::Button {
            button: MouseButton::Right,
            pressed: false,
        },
        // WM_MBUTTONDOWN / WM_MBUTTONUP
        0x0207 => MouseAction::Button {
            button: MouseButton::Middle,
            pressed: true,
        },
        0x0208 => MouseAction::Button {
            button: MouseButton::Middle,
            pressed: false,
        },
        // WM_XBUTTONDOWN / WM_XBUTTONUP
        0x020B => MouseAction::Button {
            button: if xbutton == 1 {
                MouseButton::Back
            } else {
                MouseButton::Forward
            },
            pressed: true,
        },
        0x020C => MouseAction::Button {
            button: if xbutton == 1 {
                MouseButton::Back
            } else {
                MouseButton::Forward
            },
            pressed: false,
        },
        // WM_MOUSEWHEEL / WM_MOUSEHWHEEL (deltas already 120/notch)
        0x020A => MouseAction::Scroll { dx: 0, dy: wheel },
        0x020E => MouseAction::Scroll { dx: wheel, dy: 0 },
        _ => return None,
    })
}

/// Controls a running capture: push server updates, stop it.
pub struct Handle {
    cmd: Sender<Command>,
    outgoing: Sender<Message>,
    #[cfg(windows)]
    stop_flag: Arc<AtomicBool>,
    #[cfg(windows)]
    tid: Receiver<u32>,
    #[cfg(windows)]
    thread: Option<JoinHandle<()>>,
}

/// Consumed by the hook thread; elsewhere updates are dropped.
#[cfg_attr(not(windows), allow(dead_code))]
enum Command {
    Update {
        local: Vec<Display>,
        server: ServerInfo,
        peer: Option<u64>,
    },
}

impl Handle {
    /// No hooks (other platforms, `--dry-run`): updates are dropped, but
    /// stopping after a demote still sends `Leave`, which the server treats
    /// as releasing everything (a no-op if it holds nothing).
    pub fn noop(outgoing: Sender<Message>) -> Self {
        let (cmd, _) = mpsc::channel();
        Self {
            cmd,
            outgoing,
            #[cfg(windows)]
            stop_flag: Arc::new(AtomicBool::new(false)),
            #[cfg(windows)]
            tid: mpsc::channel().1,
            #[cfg(windows)]
            thread: None,
        }
    }

    pub fn update(&self, local: Vec<Display>, server: ServerInfo, peer: Option<u64>) {
        let _ = self.cmd.send(Command::Update {
            local,
            server,
            peer,
        });
    }

    /// Stop the hooks and unhide the cursor. `send_leave` (demoted while
    /// connected): the server releases everything still held. Runs after the
    /// hook thread exited, so it sorts after its last input.
    pub fn stop(self, send_leave: bool) {
        #[cfg(windows)]
        {
            let mut this = self;
            imp::stop_thread(&this.tid, &this.stop_flag, &mut this.thread);
            if send_leave {
                let _ = this.outgoing.send(Message::Leave);
            }
        }
        #[cfg(not(windows))]
        if send_leave {
            let _ = self.outgoing.send(Message::Leave);
        }
    }
}

#[cfg(windows)]
pub mod imp {
    //! The Win32 half: hook installation, the message loop, cursor hide/park.

    use super::*;
    use std::ptr;

    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, ClipCursor, DispatchMessageW, GetMessageW, HC_ACTION, HHOOK,
        KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_UP, LLMHF_INJECTED, MSG,
        MSLLHOOKSTRUCT, PostThreadMessageW, SetCursorPos, SetWindowsHookExW, ShowCursor,
        TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_QUIT,
    };

    use crate::log;

    static STATE: Mutex<Option<ThreadState>> = Mutex::new(None);
    std::thread_local! {
        /// Hook callbacks run on our own thread; our own `SetCursorPos`
        /// re-enters the mouse proc, which must pass through, not deadlock.
        static IN_HOOK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    struct ThreadState {
        mainside: MainSide<WinCursor>,
        cmd: Receiver<Command>,
        // Raw hook handles: `HHOOK` is a raw pointer and not `Send`, so the
        // values are kept as `isize` and only cast back for Win32 calls made
        // on this same thread.
        kbd: isize,
        mouse: isize,
    }

    pub struct WinCursor;

    impl Cursor for WinCursor {
        fn went_remote(&mut self, at: (i32, i32)) {
            // SAFETY: plain Win32 calls; a 1x1 clip freezes the cursor where
            // the controller left it, hiding removes it from the screen.
            unsafe {
                let rect = RECT {
                    left: at.0,
                    top: at.1,
                    right: at.0 + 1,
                    bottom: at.1 + 1,
                };
                ClipCursor(&rect);
                while ShowCursor(0) >= 0 {}
            }
        }

        fn went_local(&mut self, at: Option<(i32, i32)>) {
            // SAFETY: plain Win32 calls; unclip first so the warp lands.
            unsafe {
                ClipCursor(ptr::null());
                if let Some((x, y)) = at {
                    SetCursorPos(x, y);
                }
                while ShowCursor(1) < 0 {}
            }
        }
    }

    /// Install the hooks on a thread with a message loop; returns the
    /// [`Handle`] driving it. The thread ends on [`Handle::stop`].
    /// Install the hooks on a thread with a message loop; returns the
    /// [`Handle`] driving it. The thread ends on [`Handle::stop`], which
    /// sends `Leave` itself when demoted.
    pub fn start(
        outgoing: Sender<Message>,
        local: Vec<Display>,
        server: ServerInfo,
        peer: u64,
    ) -> Handle {
        let (cmd, rx) = mpsc::channel();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let (tid_tx, tid_rx) = mpsc::channel();
        let flag = Arc::clone(&stop_flag);
        let thread = std::thread::Builder::new()
            .name("onemouse-capture".into())
            .spawn({
                let outgoing = outgoing.clone();
                move || run(outgoing, rx, flag, tid_tx, local, server, peer)
            })
            .expect("capture thread");
        Handle {
            cmd,
            outgoing,
            stop_flag,
            tid: tid_rx,
            thread: Some(thread),
        }
    }

    pub fn stop_thread(
        tid: &Receiver<u32>,
        stop_flag: &Arc<AtomicBool>,
        thread: &mut Option<JoinHandle<()>>,
    ) {
        stop_flag.store(true, Ordering::Release);
        // The thread reports its id before installing hooks; waking its
        // message loop ends it. A missing id means it never got that far.
        if let Ok(id) = tid.try_recv() {
            // SAFETY: plain Win32 call with our own thread id.
            unsafe { PostThreadMessageW(id, WM_QUIT, 0, 0) };
        }
        if let Some(thread) = thread.take() {
            let _ = thread.join();
        }
    }

    fn run(
        outgoing: Sender<Message>,
        cmd: Receiver<Command>,
        stop_flag: Arc<AtomicBool>,
        tid_tx: Sender<u32>,
        local: Vec<Display>,
        server: ServerInfo,
        peer: u64,
    ) {
        // SAFETY: standard low-level hook installation on our own thread;
        // unhooked again before the thread exits.
        unsafe {
            let _ = tid_tx.send(GetCurrentThreadId());
            let kbd = SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(kbd_proc),
                GetModuleHandleW(ptr::null()),
                0,
            ) as isize;
            let mouse = SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(mouse_proc),
                GetModuleHandleW(ptr::null()),
                0,
            ) as isize;
            if kbd == 0 || mouse == 0 {
                if kbd != 0 {
                    UnhookWindowsHookEx(kbd as HHOOK);
                }
                if mouse != 0 {
                    UnhookWindowsHookEx(mouse as HHOOK);
                }
                log!("capture hooks failed: {}", std::io::Error::last_os_error());
                return;
            }
            *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(ThreadState {
                mainside: MainSide::new(outgoing.clone(), WinCursor, &local, &server, peer),
                cmd,
                kbd,
                mouse,
            });
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if let Some(state) = STATE.lock().unwrap_or_else(|e| e.into_inner()).take() {
                UnhookWindowsHookEx(state.kbd as HHOOK);
                UnhookWindowsHookEx(state.mouse as HHOOK);
            }
            // Never strand a hidden/clipped cursor if anything above failed.
            // (`Leave` itself goes out from `Handle::stop`, after this join.)
            WinCursor.went_local(None);
            let _ = (outgoing, stop_flag);
        }
    }

    fn with_state(f: impl FnOnce(&mut ThreadState) -> bool) -> bool {
        if IN_HOOK.get() {
            return false;
        }
        IN_HOOK.set(true);
        let swallowed = STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .map(|state| {
                drain(state);
                f(state)
            })
            .unwrap_or(false);
        IN_HOOK.set(false);
        swallowed
    }

    fn drain(state: &mut ThreadState) {
        while let Ok(cmd) = state.cmd.try_recv() {
            match cmd {
                Command::Update {
                    local,
                    server,
                    peer,
                } => {
                    state.mainside.update(&local, &server, peer);
                }
            }
        }
    }

    unsafe extern "system" fn kbd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let mut hook: HHOOK = ptr::null_mut();
        let swallowed = (ncode == HC_ACTION as i32)
            .then(|| {
                // SAFETY: `lparam` is a `KBDLLHOOKSTRUCT` for `HC_ACTION`.
                let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
                hook = STATE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|s| s.kbd as HHOOK)
                    .unwrap_or(ptr::null_mut());
                if info.flags & LLKHF_INJECTED != 0 {
                    return None;
                }
                let up = info.flags & LLKHF_UP != 0;
                let extended = info.flags & LLKHF_EXTENDED != 0;
                Some((info.vkCode as u16, info.scanCode as u16, extended, up))
            })
            .flatten()
            .map(|(vk, scan, extended, up)| {
                with_state(|state| match key_input(vk, scan, extended, up) {
                    Some(Input::Key { code, pressed }) => state.mainside.key(Some(code), pressed),
                    _ => state.mainside.key(None, !up),
                })
            })
            .unwrap_or(false);
        if swallowed {
            return 1;
        }
        // SAFETY: forwarding to the next hook, as documented.
        unsafe { CallNextHookEx(hook, ncode, wparam, lparam) }
    }

    unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let mut hook: HHOOK = ptr::null_mut();
        let event = (ncode == HC_ACTION as i32)
            .then(|| {
                // SAFETY: `lparam` is an `MSLLHOOKSTRUCT` for `HC_ACTION`.
                let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
                hook = STATE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|s| s.mouse as HHOOK)
                    .unwrap_or(ptr::null_mut());
                if info.flags & LLMHF_INJECTED != 0 {
                    return None;
                }
                let xbutton = (info.mouseData >> 16) as u16;
                let wheel = ((info.mouseData >> 16) as u16 as i16) as i32;
                mouse_action(wparam as u32, wheel, xbutton)
                    .map(|action| (action, info.pt.x, info.pt.y))
            })
            .flatten();
        let swallowed = event
            .map(|(action, x, y)| {
                with_state(|state| match action {
                    MouseAction::Move => state.mainside.mouse_at(x, y),
                    MouseAction::Button { button, pressed } => {
                        state.mainside.mouse_button(button, pressed, x, y)
                    }
                    MouseAction::Scroll { dx, dy } => state.mainside.mouse_scroll(dx, dy, x, y),
                })
            })
            .unwrap_or(false);
        if swallowed {
            return 1;
        }
        // SAFETY: forwarding to the next hook, as documented.
        unsafe { CallNextHookEx(hook, ncode, wparam, lparam) }
    }
}

#[cfg(windows)]
pub use imp::start;

/// Elsewhere (and for `--dry-run`, via [`Handle::noop`]): no hooks, so the
/// PC never goes remote; role transitions still send `Leave` on demote.
#[cfg(not(windows))]
pub fn start(outgoing: Sender<Message>, _: Vec<Display>, _: ServerInfo, _: u64) -> Handle {
    Handle::noop(outgoing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onemouse_protocol::key;

    /// Records cursor actions for tests.
    #[derive(Debug, Default)]
    struct RecordingCursor {
        events: Vec<CursorEvent>,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum CursorEvent {
        Remote((i32, i32)),
        Local(Option<(i32, i32)>),
    }

    impl Cursor for RecordingCursor {
        fn went_remote(&mut self, at: (i32, i32)) {
            self.events.push(CursorEvent::Remote(at));
        }

        fn went_local(&mut self, at: Option<(i32, i32)>) {
            self.events.push(CursorEvent::Local(at));
        }
    }

    fn pc() -> Vec<Display> {
        vec![Display {
            id: 1,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.25,
            primary: true,
        }]
    }

    fn mac() -> Vec<Display> {
        // Points with scale 1.0: visual units map 1:1 (see module docs).
        vec![Display {
            id: 2,
            x: 0,
            y: 0,
            width: 1470,
            height: 956,
            scale: 1.0,
            primary: true,
        }]
    }

    fn server() -> ServerInfo {
        ServerInfo::new(Os::MacOs, mac())
    }

    fn mainside() -> (MainSide<RecordingCursor>, Receiver<Message>) {
        let (tx, rx) = mpsc::channel();
        let side = MainSide::new(tx, RecordingCursor::default(), &pc(), &server(), 1);
        (side, rx)
    }

    fn recv_all(rx: &Receiver<Message>) -> Vec<Message> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
        out
    }

    #[test]
    fn local_space_divides_by_the_display_scale() {
        let space = LocalSpace::new(&pc());
        assert_eq!(space.rects.len(), 1);
        // 1920 px @ 1.25 → 1536 visual units.
        assert_eq!(space.rects[0].width, 1536.0);
        assert_eq!(space.to_visual(1920, 1080), Point::new(1536.0, 864.0));
        assert_eq!(space.to_physical(Point::new(1536.0, 864.0)), (1920, 1080));
        assert_eq!(space.to_physical(Point::new(0.0, 0.0)), (0, 0));
    }

    #[test]
    fn crossing_the_left_edge_enters_in_mac_points() {
        let (mut side, rx) = mainside();
        // The Mac sits on the PC's left by default; push off the left edge.
        // PC pixels ÷ 1.25 = visual units; the Mac block spans x -1470..0.
        assert!(!side.mouse_at(10, 500)); // visual (8, 400)
        assert!(side.mouse_at(2, 500)); // visual (1.6, 400) → off the edge
        assert!(side.is_remote());
        // Entry at visual (-4.8, 400) = Mac points (1465.2, 446).
        assert_eq!(recv_all(&rx), [Message::Enter { x: 1465, y: 446 }]);
        // Cursor hidden and pinned where it left.
        assert_eq!(side.cursor.events, [CursorEvent::Remote((2, 500))]);
        // Parked (clipped) moves: swallowed, nothing new.
        assert!(side.mouse_at(2, 500));
        assert!(recv_all(&rx).is_empty());
        // Deeper into the Mac: 1 visual unit = 1 point.
        assert!(side.mouse_at(-6, 502)); // visual (-4.8, 401.6)
        assert_eq!(recv_all(&rx), [Message::MouseMove { x: 1459, y: 448 }]);
    }

    #[test]
    fn keys_translate_pc_to_mac_while_remote() {
        let (mut side, rx) = mainside();
        // Local keys pass through untouched.
        assert!(!side.key(Some(key::A), true));
        assert!(recv_all(&rx).is_empty());
        // Go remote, then Ctrl+C arrives on the Mac as Cmd+C.
        side.mouse_at(10, 500);
        side.mouse_at(2, 500);
        assert!(side.is_remote());
        let _ = recv_all(&rx);
        assert!(side.key(Some(key::LEFT_CTRL), true));
        assert!(side.key(Some(key::C), true));
        let sent = recv_all(&rx);
        assert_eq!(
            sent,
            [
                Message::Key {
                    code: key::LEFT_META,
                    pressed: true
                },
                Message::Key {
                    code: key::C,
                    pressed: true
                },
            ]
        );
        // Release order is preserved on the way out.
        assert!(side.key(Some(key::C), false));
        assert!(side.key(Some(key::LEFT_CTRL), false));
        let sent = recv_all(&rx);
        assert_eq!(
            sent,
            [
                Message::Key {
                    code: key::C,
                    pressed: false
                },
                Message::Key {
                    code: key::LEFT_META,
                    pressed: false
                },
            ]
        );
    }

    #[test]
    fn unmapped_keys_swallow_only_while_remote() {
        let (mut side, _) = mainside();
        assert!(!side.key(None, true));
        side.mouse_at(10, 500);
        side.mouse_at(2, 500);
        assert!(side.is_remote());
        assert!(side.key(None, true));
    }

    #[test]
    fn walking_back_returns_and_shows_the_cursor() {
        let (mut side, rx) = mainside();
        side.mouse_at(10, 500);
        side.mouse_at(2, 500);
        assert!(side.is_remote());
        let _ = recv_all(&rx);
        side.cursor.events.clear();
        // Push back through the Mac's right edge onto the PC.
        assert!(side.mouse_at(200, 420));
        assert!(!side.is_remote());
        assert_eq!(recv_all(&rx), [Message::Leave]);
        assert!(matches!(
            side.cursor.events.as_slice(),
            [CursorEvent::Local(_)]
        ));
        // Local input passes through again.
        assert!(!side.key(Some(key::A), true));
    }

    #[test]
    fn mouse_action_maps_the_hook_constants() {
        assert_eq!(mouse_action(0x0200, 0, 0), Some(MouseAction::Move));
        assert_eq!(
            mouse_action(0x0201, 0, 0),
            Some(MouseAction::Button {
                button: MouseButton::Left,
                pressed: true
            })
        );
        assert_eq!(
            mouse_action(0x020B, 0, 1),
            Some(MouseAction::Button {
                button: MouseButton::Back,
                pressed: true
            })
        );
        assert_eq!(
            mouse_action(0x020C, 0, 2),
            Some(MouseAction::Button {
                button: MouseButton::Forward,
                pressed: false
            })
        );
        assert_eq!(
            mouse_action(0x020A, 120, 0),
            Some(MouseAction::Scroll { dx: 0, dy: 120 })
        );
        assert_eq!(mouse_action(0xFFFF, 0, 0), None);
    }

    #[test]
    fn key_input_maps_hook_events() {
        assert_eq!(
            key_input(0, 0x1E, false, false),
            Some(Input::Key {
                code: key::A,
                pressed: true
            })
        );
        assert_eq!(
            key_input(0, 0x1E, false, true),
            Some(Input::Key {
                code: key::A,
                pressed: false
            })
        );
        assert_eq!(key_input(0, 0x00, false, false), None);
    }
}
