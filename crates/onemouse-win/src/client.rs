//! Connection to the primary: discovery, encrypted handshake and pairing,
//! `Hello`, apply incoming messages, heartbeat, display updates, reconnect.
//! Portable (tested on every CI runner); the Windows specifics come in
//! through [`Host`] and the injector [`Backend`].

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use onemouse_core::layout::Point;
use onemouse_protocol::{
    Display, FrameError, Hello, Main, Message, Os, PROTOCOL_VERSION, read_message, write_message,
};

use onemouse_transport::{Identity, PairingRequest, TrustStore, discovery, fingerprint};

use crate::capture::{self, ServerInfo};
use crate::inject::{Backend, Injector};
use crate::log;

/// What the client needs from the machine it runs on.
pub trait Host: Send + Sync + 'static {
    fn name(&self) -> String;
    fn displays(&self) -> Vec<Display>;
    /// Changes whenever the OS reports a display change. Displays are also
    /// re-checked on every ping, so this only makes updates faster.
    fn display_generation(&self) -> u64 {
        0
    }
    fn status(&self, _status: &str) {}
    /// The persisted "keyboard & mouse are on" setting. Defaults to the
    /// server (today's behavior); the server's is authoritative at connect.
    fn main_setting(&self) -> Main {
        Main::Server
    }
    fn set_main_setting(&self, _main: Main) {}
}

/// A host with a fixed name and display layout (`--dry-run`, tests).
#[derive(Debug, Clone)]
pub struct StaticHost {
    pub name: String,
    pub displays: Vec<Display>,
}

impl Host for StaticHost {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn displays(&self) -> Vec<Display> {
        self.displays.clone()
    }
}

pub type SharedInjector<B> = Arc<Mutex<Injector<B>>>;

pub fn lock<B>(injector: &SharedInjector<B>) -> MutexGuard<'_, Injector<B>> {
    // A panic elsewhere must never stop us from releasing keys.
    injector.lock().unwrap_or_else(|e| e.into_inner())
}

/// Who we are and whom we trust.
pub struct Security {
    pub identity: Identity,
    pub trust: Mutex<TrustStore>,
    /// Asks the user whether the pairing code matches the Mac's.
    pub confirm: Box<dyn Fn(&PairingRequest) -> bool + Send + Sync>,
}

impl fmt::Debug for Security {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Security")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// The primary's address; `None` finds it over mDNS.
    pub host: Option<String>,
    pub port: u16,
    pub security: Arc<Security>,
    /// How long to browse for the primary.
    pub discovery_timeout: Duration,
    pub connect_timeout: Duration,
    pub ping_interval: Duration,
    /// Drop the connection when nothing arrives for this long.
    pub silence_timeout: Duration,
    /// How often the writer checks for display changes and pings.
    pub poll_interval: Duration,
}

impl Config {
    pub fn new(host: Option<String>, port: u16, security: Arc<Security>) -> Self {
        Self {
            host,
            port,
            security,
            discovery_timeout: Duration::from_secs(3),
            connect_timeout: Duration::from_secs(5),
            ping_interval: Duration::from_secs(2),
            silence_timeout: Duration::from_secs(6),
            poll_interval: Duration::from_millis(250),
        }
    }
}

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Frame(FrameError),
    Rejected(String),
    Protocol(String),
    Transport(onemouse_transport::Error),
    Discovery(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Frame(FrameError::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                write!(f, "primary went silent")
            }
            Self::Frame(FrameError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                write!(f, "primary closed the connection")
            }
            Self::Frame(e) => write!(f, "{e}"),
            Self::Rejected(reason) => write!(f, "rejected by primary: {reason}"),
            Self::Protocol(what) => write!(f, "protocol error: {what}"),
            Self::Transport(e) => write!(f, "{e}"),
            Self::Discovery(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<onemouse_transport::Error> for ClientError {
    fn from(e: onemouse_transport::Error) -> Self {
        Self::Transport(e)
    }
}

impl From<FrameError> for ClientError {
    fn from(e: FrameError) -> Self {
        Self::Frame(e)
    }
}

/// Exponential reconnect delay.
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
    min: Duration,
    max: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        Self {
            next: min,
            min,
            max,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }

    pub fn reset(&mut self) {
        self.next = self.min;
    }
}

/// Who has the keyboard and mouse, shared by the session (which adopts the
/// server's word: `Welcome.main`, `SetMain`) and the UI (the user's word).
/// Owns the main-side capture while this PC is main.
///
/// Lock order: `state`, then `capture`, then `outgoing`. Every send uses a
/// sender cloned out under its lock; the channel is unbounded, so sending
/// never blocks while holding a lock.
pub struct Role<H: Host> {
    host: Arc<H>,
    state: Mutex<State>,
    capture: Mutex<Option<capture::Handle>>,
    outgoing: Mutex<Option<Sender<Message>>>,
    next_peer: AtomicU64,
    capture_enabled: bool,
}

struct State {
    main: Main,
    server: ServerInfo,
    local: Vec<Display>,
    /// Connection the capture belongs to; bumped on every connect so a
    /// reconnect while remote re-enters cleanly.
    peer: u64,
}

impl<H: Host> Role<H> {
    pub fn new(host: &Arc<H>, capture_enabled: bool) -> Self {
        let main = host.main_setting();
        Self {
            host: Arc::clone(host),
            state: Mutex::new(State {
                main,
                server: ServerInfo::new(Os::Windows, Vec::new()),
                local: host.displays(),
                peer: 0,
            }),
            capture: Mutex::new(None),
            outgoing: Mutex::new(None),
            next_peer: AtomicU64::new(1),
            capture_enabled,
        }
    }

    pub fn get(&self) -> Main {
        guard(&self.state).main
    }

    /// The user's choice (tray menu): adopt it, persist it and tell the
    /// server. Never echoes back what the server sent: [`adopt`] is for that.
    pub fn request(&self, main: Main) {
        if !self.adopt(main) {
            return;
        }
        if let Some(tx) = self.outgoing() {
            let _ = tx.send(Message::SetMain { main });
        }
    }

    /// Adopt `main` (from `Welcome` or `SetMain`): persist, restart the
    /// capture, no echo. Returns whether anything changed. A side that stops
    /// being main tells the server to release everything via `Leave`.
    pub fn adopt(&self, main: Main) -> bool {
        let mut state = guard(&self.state);
        if state.main == main {
            return false;
        }
        if state.main == Main::Client
            && let Some(handle) = guard(&self.capture).take()
        {
            handle.stop(true);
        }
        state.main = main;
        self.host.set_main_setting(main);
        self.sync_capture(&state);
        true
    }

    /// Connected: `Welcome.main` wins, the writer takes this `outgoing`,
    /// and the capture (re)starts on a fresh peer id when we are main.
    pub fn on_connect(&self, outgoing: Sender<Message>, main: Main, server: ServerInfo) {
        let peer = self.next_peer.fetch_add(1, Ordering::SeqCst);
        let mut state = guard(&self.state);
        if let Some(handle) = guard(&self.capture).take() {
            // A new connection never saw `Enter`: don't `Leave` it, just
            // stop. (If we are still main the capture restarts below.)
            handle.stop(false);
        }
        if state.main != main {
            state.main = main;
            self.host.set_main_setting(main);
        }
        state.server = server;
        state.local = self.host.displays();
        state.peer = peer;
        *guard(&self.outgoing) = Some(outgoing);
        self.sync_capture(&state);
    }

    /// The session ended: nothing can be sent anymore; park the cursor
    /// without telling a dead socket.
    pub fn on_disconnect(&self) {
        *guard(&self.outgoing) = None;
        if let Some(handle) = guard(&self.capture).take() {
            handle.stop(false);
        }
    }

    /// The server's displays changed: keep capturing against the new ones.
    pub fn on_server_displays(&self, displays: Vec<Display>) {
        self.push_update(|server| server.displays = displays);
    }

    /// The server rearranged the client's desktop in its own coordinates.
    pub fn on_arrangement(&self, origin: Point) {
        self.push_update(|server| server.arrangement = Some(origin));
    }

    /// Our own displays changed (polled by the writer): same treatment.
    pub fn on_local_displays(&self, local: Vec<Display>) {
        let mut state = guard(&self.state);
        state.local = local.clone();
        let (server, peer) = (state.server.clone(), state.peer);
        if let Some(handle) = guard(&self.capture).as_ref() {
            handle.update(local, server, Some(peer));
        }
    }

    fn push_update(&self, update: impl FnOnce(&mut ServerInfo)) {
        let mut state = guard(&self.state);
        update(&mut state.server);
        let (server, local, peer) = (state.server.clone(), state.local.clone(), state.peer);
        if let Some(handle) = guard(&self.capture).as_ref() {
            handle.update(local, server, Some(peer));
        }
    }

    /// (Re)start the capture when we are main and connected, else make sure
    /// it is stopped. Callers hold `state`.
    fn sync_capture(&self, state: &State) {
        if state.main != Main::Client {
            return;
        }
        let Some(tx) = self.outgoing() else { return };
        let handle = if self.capture_enabled {
            capture::start(tx, state.local.clone(), state.server.clone(), state.peer)
        } else {
            capture::Handle::noop(tx)
        };
        *guard(&self.capture) = Some(handle);
    }

    fn outgoing(&self) -> Option<Sender<Message>> {
        guard(&self.outgoing).clone()
    }

    #[cfg(test)]
    pub fn is_capturing(&self) -> bool {
        guard(&self.capture).is_some()
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> (Main, ServerInfo) {
        let state = guard(&self.state);
        (state.main, state.server.clone())
    }
}

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Connects forever, reconnecting with backoff. Everything held is released
/// whenever a session ends, however it ends.
pub fn run<H: Host, B: Backend + Send + 'static>(
    config: &Config,
    host: Arc<H>,
    injector: SharedInjector<B>,
    role: &Arc<Role<H>>,
) -> ! {
    let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(30));
    loop {
        let started = Instant::now();
        let mut welcomed = false;
        match run_session(config, &host, &injector, role, || welcomed = true) {
            Ok(()) => {
                log!("disconnected");
                host.status("Disconnected, reconnecting…");
            }
            Err(e) => {
                log!("connection ended: {e}");
                host.status(&format!("Not connected: {e}"));
            }
        }
        lock(&injector).release_all();
        if welcomed && started.elapsed() > Duration::from_secs(10) {
            backoff.reset();
        }
        let delay = backoff.next_delay();
        log!("reconnecting in {:.1} s", delay.as_secs_f32());
        thread::sleep(delay);
    }
}

/// One connection: handshake, then apply messages until it ends. Releases
/// everything held before returning, on every path. `on_welcome` runs once
/// the primary accepted us.
pub fn run_session<H: Host, B: Backend + Send + 'static>(
    config: &Config,
    host: &Arc<H>,
    injector: &SharedInjector<B>,
    role: &Arc<Role<H>>,
    on_welcome: impl FnOnce(),
) -> Result<(), ClientError> {
    let mut on_welcome = Some(on_welcome);
    let mut last_err = None;
    // Several candidates when discovery sees more than one answer for our
    // Mac (e.g. someone else advertising its fingerprint): the handshake
    // tells the real one apart, so failures before Welcome move on.
    for addr in candidates(config)? {
        let tcp = match TcpStream::connect_timeout(&addr, config.connect_timeout) {
            Ok(tcp) => tcp,
            Err(e) => {
                last_err = Some(e.into());
                continue;
            }
        };
        let mut welcomed = false;
        let result = session(config, host, injector, role, &tcp, || {
            welcomed = true;
            if let Some(f) = on_welcome.take() {
                f();
            }
        });
        lock(injector).release_all();
        role.on_disconnect();
        let _ = tcp.shutdown(Shutdown::Both);
        match result {
            Err(e) if !welcomed => {
                log!("{addr}: {e}");
                last_err = Some(e);
            }
            other => return other,
        }
    }
    Err(last_err.unwrap_or_else(|| {
        ClientError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "host resolved to no address",
        ))
    }))
}

fn candidates(config: &Config) -> Result<Vec<SocketAddr>, ClientError> {
    match &config.host {
        Some(host) => Ok((host.as_str(), config.port).to_socket_addrs()?.collect()),
        None => discover(config),
    }
}

/// Finds the primary over mDNS.
fn discover(config: &Config) -> Result<Vec<SocketAddr>, ClientError> {
    let pinned: Vec<String> = config
        .security
        .trust
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .peers()
        .iter()
        .map(|p| fingerprint(&p.key))
        .collect();
    let found = discovery::browse(config.discovery_timeout, pinned.first().map(String::as_str))
        .map_err(|e| ClientError::Discovery(format!("mDNS browse failed: {e}")))?;
    let chosen = choose(&found, &pinned).map_err(ClientError::Discovery)?;
    for f in &chosen {
        log!("found {} at {}", f.name, display_addrs(&f.addrs));
    }
    Ok(chosen
        .iter()
        .flat_map(|f| f.addrs.iter().map(|ip| SocketAddr::new(*ip, f.port)))
        .collect())
}

/// Which advertisements to try, in order: every one claiming a paired Mac's
/// fingerprint; else, if all compatible ones claim the same fingerprint (the
/// usual first-pairing case), all of them; else none (ambiguous).
fn choose<'a>(
    found: &'a [discovery::Found],
    pinned: &[String],
) -> Result<Vec<&'a discovery::Found>, String> {
    let compatible: Vec<_> = found
        .iter()
        .filter(|f| f.version == Some(PROTOCOL_VERSION) && !f.addrs.is_empty())
        .collect();
    let paired: Vec<_> = compatible
        .iter()
        .copied()
        .filter(|f| pinned.contains(&f.fingerprint))
        .collect();
    if !paired.is_empty() {
        return Ok(paired);
    }
    let mut fingerprints: Vec<_> = compatible.iter().map(|f| &f.fingerprint).collect();
    fingerprints.dedup();
    if fingerprints.len() == 1 {
        return Ok(compatible);
    }
    let seen: Vec<_> = found
        .iter()
        .map(|f| format!("{} (v{})", f.name, f.version.unwrap_or(0)))
        .collect();
    Err(if seen.is_empty() {
        "no Mac found on the network; is onemouse running there? (or pass --host)".into()
    } else {
        format!(
            "can't tell which Mac to use: found {}; pass --host",
            seen.join(", ")
        )
    })
}

fn display_addrs(addrs: &[IpAddr]) -> String {
    addrs
        .iter()
        .map(IpAddr::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn session<H: Host, B: Backend + Send + 'static>(
    config: &Config,
    host: &Arc<H>,
    injector: &SharedInjector<B>,
    role: &Arc<Role<H>>,
    tcp: &TcpStream,
    on_welcome: impl FnOnce(),
) -> Result<(), ClientError> {
    tcp.set_nodelay(true)?;

    let name = host.name();
    let security = &config.security;
    let opts = onemouse_transport::Options {
        can_pair: true,
        confirm: &*security.confirm,
        ..onemouse_transport::Options::new(&security.identity, &name, &security.trust)
    };
    let (secure, peer) = onemouse_transport::connect(tcp.try_clone()?, &opts)?;
    if peer.newly_paired {
        log!("paired with {} (key {})", peer.name, peer.fingerprint());
    }
    // On the reader's own handle: Windows doesn't share socket options
    // between cloned handles.
    secure.set_read_timeout(Some(config.silence_timeout))?;
    let (mut reader, mut writer) = secure.split();

    let generation = host.display_generation();
    let displays = host.displays();
    write_message(
        &mut writer,
        &Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            name,
            os: Os::Windows,
            displays: displays.clone(),
        }),
    )?;

    let (tx, rx) = mpsc::channel();
    match read_message(&mut reader)? {
        Message::Welcome {
            protocol_version,
            name,
            os,
            displays: server_displays,
            main,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                return Err(ClientError::Protocol(format!(
                    "primary speaks v{protocol_version}, we speak v{PROTOCOL_VERSION}"
                )));
            }
            log!(
                "connected to {name} ({} display(s) reported), main: {main:?}",
                displays.len()
            );
            host.status(&format!("Connected to {name}"));
            // The server's setting is authoritative at connect.
            role.on_connect(tx.clone(), main, ServerInfo::new(os, server_displays));
        }
        Message::Reject { reason } => return Err(ClientError::Rejected(reason)),
        other => {
            return Err(ClientError::Protocol(format!(
                "expected Welcome, got {other:?}"
            )));
        }
    }
    on_welcome();

    let writer = {
        let tcp = tcp.try_clone()?;
        let host = Arc::clone(host);
        let role = Arc::clone(role);
        let config = config.clone();
        thread::Builder::new()
            .name("onemouse-writer".into())
            .spawn(move || {
                write_loop(
                    writer, &tcp, rx, &*host, &role, &config, generation, displays,
                )
            })?
    };

    let result = read_loop(&mut reader, injector, &tx, role);

    // Stop the writer: closing the channel ends its loop, shutting the socket
    // down unblocks a stuck write. Role (and the capture it owns) still hold
    // clones of the sender, so release them first: otherwise the channel
    // never disconnects and `join` below hangs forever.
    role.on_disconnect();
    drop(tx);
    let _ = tcp.shutdown(Shutdown::Both);
    let _ = writer.join();
    result
}

fn read_loop<H: Host, B: Backend>(
    stream: &mut impl Read,
    injector: &SharedInjector<B>,
    tx: &Sender<Message>,
    role: &Role<H>,
) -> Result<(), ClientError> {
    let mut session = Session::default();
    let send = |msg: Message| {
        tx.send(msg).map_err(|_| {
            ClientError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped"))
        })
    };
    loop {
        match read_message(stream)? {
            Message::Ping(id) => send(Message::Pong(id))?,
            Message::Pong(_) => {}
            // The main side sends input; when we are main the server
            // shouldn't send any. Anything arriving then is a race at the
            // switch: drop it (releases are already covered by Leave).
            input @ (Message::Enter { .. }
            | Message::Leave
            | Message::MouseMove { .. }
            | Message::MouseButton { .. }
            | Message::Scroll { .. }
            | Message::Key { .. }) => {
                if role.get() == Main::Client {
                    log!("ignoring {input:?}: this PC is main");
                } else {
                    session.handle(input, &mut lock(injector))?;
                }
            }
            Message::DisplaysChanged { displays } => {
                log!("server displays changed ({} display(s))", displays.len());
                role.on_server_displays(displays);
            }
            Message::Arrangement { x, y } => {
                log!("server arrangement: origin ({x}, {y})");
                role.on_arrangement(Point::new(f64::from(x), f64::from(y)));
            }
            Message::SetMain { main } => {
                log!("main is now {main:?}");
                // Adopted and persisted, never echoed.
                role.adopt(main);
            }
            Message::Reject { reason } => return Err(ClientError::Rejected(reason)),
            Message::Hello(_) | Message::Welcome { .. } => {
                return Err(ClientError::Protocol("unexpected message".into()));
            }
        }
    }
}

/// Sends replies, pings and display updates. The only thread writing to the
/// socket after the handshake. Also keeps the main-side capture on the
/// current local displays.
#[allow(clippy::too_many_arguments)] // thread entry: one spawn site, no good grouping
fn write_loop<H: Host>(
    mut stream: impl Write,
    tcp: &TcpStream,
    rx: mpsc::Receiver<Message>,
    host: &H,
    role: &Role<H>,
    config: &Config,
    mut generation: u64,
    mut displays: Vec<Display>,
) {
    let mut next_ping = Instant::now() + config.ping_interval;
    let mut ping_id = 0u64;
    let result: Result<(), FrameError> = (|| {
        loop {
            match rx.recv_timeout(config.poll_interval) {
                Ok(msg) => write_message(&mut stream, &msg)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            let ping_due = Instant::now() >= next_ping;
            let current = host.display_generation();
            if ping_due || current != generation {
                generation = current;
                let now = host.displays();
                if now != displays {
                    log!("displays changed ({} display(s))", now.len());
                    displays = now.clone();
                    role.on_local_displays(now.clone());
                    write_message(&mut stream, &Message::DisplaysChanged { displays: now })?;
                }
            }
            if ping_due {
                ping_id = ping_id.wrapping_add(1);
                write_message(&mut stream, &Message::Ping(ping_id))?;
                next_ping = Instant::now() + config.ping_interval;
            }
        }
    })();
    if let Err(e) = result {
        log!("write failed: {e}");
        // Wake the reader so the session ends now, not at the silence timeout.
        let _ = tcp.shutdown(Shutdown::Both);
    }
}

/// Applies main → remote input messages to the injector. Only fed while we
/// are remote; role messages (`SetMain`, …) never reach it.
#[derive(Debug, Default)]
pub struct Session {
    /// Whether the cursor is on this machine (between `Enter` and `Leave`).
    /// Input outside that window is ignored.
    entered: bool,
}

impl Session {
    /// Injection failures (e.g. UIPI blocking input to an elevated window)
    /// are logged, not fatal.
    pub fn handle<B: Backend>(
        &mut self,
        msg: Message,
        inj: &mut Injector<B>,
    ) -> Result<(), ClientError> {
        let result = match msg {
            Message::Enter { x, y } => {
                log!("cursor entered at ({x}, {y})");
                // Nothing is held on entry: clear anything left from before.
                inj.release_all();
                self.entered = true;
                inj.move_to(x, y)
            }
            Message::Leave => {
                log!("cursor left");
                inj.release_all();
                self.entered = false;
                Ok(())
            }
            Message::MouseMove { x, y } if self.entered => inj.move_to(x, y),
            Message::MouseButton { button, pressed } if self.entered => inj.button(button, pressed),
            Message::Scroll { dx, dy } if self.entered => inj.scroll(dx, dy),
            Message::Key { code, pressed } if self.entered => inj.key(code, pressed),
            Message::Key { pressed: false, .. } | Message::MouseButton { pressed: false, .. } => {
                // Can't be holding anything outside Enter…Leave.
                Ok(())
            }
            Message::MouseMove { .. }
            | Message::MouseButton { .. }
            | Message::Scroll { .. }
            | Message::Key { .. } => {
                log!("ignoring {msg:?}: cursor is not on this machine");
                Ok(())
            }
            other => {
                return Err(ClientError::Protocol(format!("unexpected {other:?}")));
            }
        };
        if let Err(e) = result {
            log!("injection failed: {e}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::{Recorded, RecordingBackend};
    use crate::keymap;
    use onemouse_protocol::{KeyCode, MouseButton, key};
    use onemouse_transport::SecureStream;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestHost {
        displays: Mutex<Vec<Display>>,
        generation: AtomicU64,
        main: Mutex<Main>,
    }

    impl Host for TestHost {
        fn name(&self) -> String {
            "test-pc".into()
        }

        fn displays(&self) -> Vec<Display> {
            self.displays.lock().unwrap().clone()
        }

        fn display_generation(&self) -> u64 {
            self.generation.load(Ordering::SeqCst)
        }

        fn main_setting(&self) -> Main {
            *self.main.lock().unwrap()
        }

        fn set_main_setting(&self, main: Main) {
            *self.main.lock().unwrap() = main;
        }
    }

    fn display(x: i32) -> Display {
        Display {
            id: 7,
            x,
            y: 0,
            width: 2560,
            height: 1440,
            scale: 1.5,
            primary: true,
        }
    }

    /// The fake primary's side of the encrypted transport.
    struct Mac {
        identity: Identity,
        trust: Mutex<TrustStore>,
        can_pair: bool,
        /// Its own, so it doesn't compete with the PC side for the
        /// process-wide slot.
        slot: onemouse_transport::PairingSlot,
    }

    struct Harness {
        listener: TcpListener,
        config: Config,
        mac: Mac,
        host: Arc<TestHost>,
        injector: SharedInjector<RecordingBackend>,
        role: Arc<Role<TestHost>>,
    }

    impl Harness {
        /// PC and Mac already paired.
        fn new() -> Self {
            let h = Self::unpaired(|_| false);
            h.mac
                .trust
                .lock()
                .unwrap()
                .pin("test-pc", h.config.security.identity.public_key())
                .unwrap();
            h.config
                .security
                .trust
                .lock()
                .unwrap()
                .pin("macbook", h.mac.identity.public_key())
                .unwrap();
            h
        }

        fn unpaired(confirm: impl Fn(&PairingRequest) -> bool + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let security = Arc::new(Security {
                identity: Identity::generate().unwrap(),
                trust: Mutex::new(TrustStore::in_memory()),
                confirm: Box::new(confirm),
            });
            let port = listener.local_addr().unwrap().port();
            let mut config = Config::new(Some("127.0.0.1".into()), port, security);
            config.ping_interval = Duration::from_secs(60);
            config.poll_interval = Duration::from_millis(10);
            let host = Arc::new(TestHost {
                displays: Mutex::new(vec![display(0)]),
                generation: AtomicU64::new(0),
                main: Mutex::new(Main::Server),
            });
            // Stub capture off-test-platform: transitions still behave.
            let role = Arc::new(Role::new(&host, false));
            Self {
                listener,
                config,
                mac: Mac {
                    identity: Identity::generate().unwrap(),
                    trust: Mutex::new(TrustStore::in_memory()),
                    can_pair: false,
                    slot: onemouse_transport::PairingSlot::new(),
                },
                host,
                injector: Arc::new(Mutex::new(Injector::new(RecordingBackend::default()))),
                role,
            }
        }

        fn spawn_client(&self) -> thread::JoinHandle<Result<(), ClientError>> {
            let (config, host, injector, role) = (
                self.config.clone(),
                Arc::clone(&self.host),
                Arc::clone(&self.injector),
                Arc::clone(&self.role),
            );
            thread::spawn(move || run_session(&config, &host, &injector, &role, || {}))
        }

        /// The fake primary accepts and runs the encrypted handshake.
        fn accept(&self) -> Result<SecureStream, onemouse_transport::Error> {
            let (tcp, _) = self.listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let confirm = |_: &PairingRequest| true;
            let opts = onemouse_transport::Options {
                can_pair: self.mac.can_pair,
                confirm: &confirm,
                pairing_slot: &self.mac.slot,
                ..onemouse_transport::Options::new(&self.mac.identity, "macbook", &self.mac.trust)
            };
            onemouse_transport::accept(tcp, &opts).map(|(secure, _)| secure)
        }

        /// Runs a session in the background, returns the primary's end of the
        /// encrypted connection after `Hello`.
        fn start(&self) -> (SecureStream, thread::JoinHandle<Result<(), ClientError>>) {
            let client = self.spawn_client();
            let mut primary = self.accept().unwrap();
            let Message::Hello(hello) = read_message(&mut primary).unwrap() else {
                panic!("expected Hello");
            };
            assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
            assert_eq!(hello.name, "test-pc");
            assert_eq!(hello.os, Os::Windows);
            assert_eq!(hello.displays, vec![display(0)]);
            (primary, client)
        }

        fn events(&self) -> Vec<Recorded> {
            lock(&self.injector).backend().events.clone()
        }
    }

    fn mac_display() -> Display {
        Display {
            id: 2,
            x: 0,
            y: 0,
            width: 1470,
            height: 956,
            scale: 1.0,
            primary: true,
        }
    }

    fn welcome(primary: &mut SecureStream) {
        welcome_as(primary, Main::Server);
    }

    fn welcome_as(primary: &mut SecureStream, main: Main) {
        write_message(
            primary,
            &Message::Welcome {
                protocol_version: PROTOCOL_VERSION,
                name: "macbook".into(),
                os: Os::MacOs,
                displays: vec![mac_display()],
                main,
            },
        )
        .unwrap();
    }

    fn send(primary: &mut SecureStream, msgs: &[Message]) {
        for msg in msgs {
            write_message(primary, msg).unwrap();
        }
    }

    fn k(code: KeyCode) -> keymap::KeyTarget {
        keymap::lookup(code).unwrap()
    }

    #[test]
    fn applies_input_and_releases_on_disconnect() {
        let h = Harness::new();
        let (mut primary, client) = h.start();
        welcome(&mut primary);
        send(
            &mut primary,
            &[
                // Before Enter: ignored.
                Message::Key {
                    code: key::A,
                    pressed: true,
                },
                Message::Enter { x: 10, y: 20 },
                Message::Key {
                    code: key::LEFT_SHIFT,
                    pressed: true,
                },
                Message::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                },
                Message::MouseMove { x: -5, y: 900 },
                Message::Scroll { dx: 0, dy: -120 },
                Message::Ping(42),
            ],
        );
        assert_eq!(read_message(&mut primary).unwrap(), Message::Pong(42));
        drop(primary);

        let err = client.join().unwrap().unwrap_err();
        assert!(
            matches!(&err, ClientError::Frame(FrameError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof),
            "{err:?}"
        );
        assert_eq!(
            h.events(),
            [
                Recorded::Move(10, 20),
                Recorded::Key(k(key::LEFT_SHIFT), true),
                Recorded::Button(MouseButton::Left, true),
                Recorded::Move(-5, 900),
                Recorded::Wheel(-120),
                Recorded::Key(k(key::LEFT_SHIFT), false),
                Recorded::Button(MouseButton::Left, false),
            ]
        );
    }

    #[test]
    fn leave_releases_and_stops_input() {
        let h = Harness::new();
        let (mut primary, client) = h.start();
        welcome(&mut primary);
        send(
            &mut primary,
            &[
                Message::Enter { x: 1, y: 1 },
                Message::Key {
                    code: key::LEFT_META,
                    pressed: true,
                },
                Message::Leave,
                Message::MouseMove { x: 50, y: 50 },
                Message::Key {
                    code: key::LEFT_META,
                    pressed: false,
                },
                Message::Ping(1),
            ],
        );
        assert_eq!(read_message(&mut primary).unwrap(), Message::Pong(1));
        assert_eq!(
            h.events(),
            [
                Recorded::Move(1, 1),
                Recorded::Key(k(key::LEFT_META), true),
                Recorded::Key(k(key::LEFT_META), false),
            ]
        );
        drop(primary);
        client.join().unwrap().unwrap_err();
    }

    #[test]
    fn enter_forgets_held_keys() {
        let h = Harness::new();
        let (mut primary, client) = h.start();
        welcome(&mut primary);
        send(
            &mut primary,
            &[
                Message::Enter { x: 0, y: 0 },
                Message::Key {
                    code: key::LEFT_ALT,
                    pressed: true,
                },
                // A second Enter without Leave in between.
                Message::Enter { x: 3, y: 4 },
                Message::Ping(2),
            ],
        );
        assert_eq!(read_message(&mut primary).unwrap(), Message::Pong(2));
        assert_eq!(
            h.events(),
            [
                Recorded::Move(0, 0),
                Recorded::Key(k(key::LEFT_ALT), true),
                Recorded::Key(k(key::LEFT_ALT), false),
                Recorded::Move(3, 4),
            ]
        );
        drop(primary);
        client.join().unwrap().unwrap_err();
    }

    #[test]
    fn reject_ends_the_session() {
        let h = Harness::new();
        let (mut primary, client) = h.start();
        send(
            &mut primary,
            &[Message::Reject {
                reason: "version mismatch".into(),
            }],
        );
        let err = client.join().unwrap().unwrap_err();
        assert!(matches!(&err, ClientError::Rejected(r) if r == "version mismatch"));
    }

    #[test]
    fn wrong_version_welcome_is_refused() {
        let h = Harness::new();
        let (mut primary, client) = h.start();
        send(
            &mut primary,
            &[Message::Welcome {
                protocol_version: PROTOCOL_VERSION + 1,
                name: "future".into(),
                os: Os::MacOs,
                displays: vec![mac_display()],
                main: Main::Server,
            }],
        );
        let err = client.join().unwrap().unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)), "{err:?}");
    }

    #[test]
    fn welcome_adopts_main_and_tracks_the_server() {
        let h = Harness::new();
        let (mut primary, client) = h.start();
        welcome_as(&mut primary, Main::Client);
        // The Pong proves Welcome was adopted (it is handled first).
        send(&mut primary, &[Message::Ping(1)]);
        assert_eq!(read_message(&mut primary).unwrap(), Message::Pong(1));
        assert_eq!(h.role.get(), Main::Client);
        assert!(h.role.is_capturing());
        let (main, server) = h.role.snapshot();
        assert_eq!(main, Main::Client);
        assert_eq!(server.os, Os::MacOs);
        assert_eq!(server.displays, vec![mac_display()]);
        // Persisted for the next start.
        assert_eq!(h.host.main_setting(), Main::Client);
        drop(primary);
        client.join().unwrap().unwrap_err();
        // Disconnect parks the capture without telling a dead socket.
        assert!(!h.role.is_capturing());
    }

    #[test]
    fn setmain_is_adopted_not_echoed_and_request_sends() {
        let h = Harness::new();
        let (mut primary, client) = h.start();
        welcome(&mut primary);
        assert_eq!(h.role.get(), Main::Server);
        // Incoming SetMain: adopted and persisted, never echoed (the Pong
        // would have an echo in front of it).
        send(&mut primary, &[Message::SetMain { main: Main::Client }]);
        send(&mut primary, &[Message::Ping(2)]);
        assert_eq!(read_message(&mut primary).unwrap(), Message::Pong(2));
        assert_eq!(h.role.get(), Main::Client);
        assert_eq!(h.host.main_setting(), Main::Client);
        assert!(h.role.is_capturing());
        // The user's own choice goes out: Leave first, then SetMain.
        h.role.request(Main::Server);
        assert_eq!(read_message(&mut primary).unwrap(), Message::Leave);
        assert_eq!(
            read_message(&mut primary).unwrap(),
            Message::SetMain { main: Main::Server }
        );
        assert!(!h.role.is_capturing());
        // Repeating the current setting sends nothing.
        h.role.request(Main::Server);
        send(&mut primary, &[Message::Ping(3)]);
        assert_eq!(read_message(&mut primary).unwrap(), Message::Pong(3));
        drop(primary);
        client.join().unwrap().unwrap_err();
    }

    #[test]
    fn arrangement_and_server_displays_are_tracked() {
        let h = Harness::new();
        let (mut primary, client) = h.start();
        welcome_as(&mut primary, Main::Client);
        send(
            &mut primary,
            &[
                Message::Arrangement { x: -1470, y: -46 },
                Message::DisplaysChanged {
                    displays: vec![mac_display(), mac_display()],
                },
                Message::Ping(4),
            ],
        );
        assert_eq!(read_message(&mut primary).unwrap(), Message::Pong(4));
        let (_, server) = h.role.snapshot();
        assert_eq!(server.arrangement, Some(Point::new(-1470.0, -46.0)));
        assert_eq!(server.displays.len(), 2);
        drop(primary);
        client.join().unwrap().unwrap_err();
    }

    #[test]
    fn input_from_the_server_is_ignored_while_we_are_main() {
        let h = Harness::new();
        let (mut primary, client) = h.start();
        welcome_as(&mut primary, Main::Client);
        send(
            &mut primary,
            &[
                Message::Enter { x: 1, y: 1 },
                Message::Key {
                    code: key::A,
                    pressed: true,
                },
                Message::MouseMove { x: 50, y: 50 },
                Message::Ping(5),
            ],
        );
        assert_eq!(read_message(&mut primary).unwrap(), Message::Pong(5));
        assert!(h.events().is_empty());
        drop(primary);
        client.join().unwrap().unwrap_err();
    }

    #[test]
    fn role_request_offline_persists_without_capturing() {
        let h = Harness::new();
        h.role.request(Main::Client);
        assert_eq!(h.role.get(), Main::Client);
        assert_eq!(h.host.main_setting(), Main::Client);
        // Nowhere to send and no server yet: stays stopped.
        assert!(!h.role.is_capturing());
    }

    #[test]
    fn silent_primary_is_dropped_and_keys_released() {
        let mut h = Harness::new();
        h.config.silence_timeout = Duration::from_millis(200);
        let (mut primary, client) = h.start();
        welcome(&mut primary);
        send(
            &mut primary,
            &[
                Message::Enter { x: 0, y: 0 },
                Message::Key {
                    code: key::RIGHT_CTRL,
                    pressed: true,
                },
            ],
        );
        let started = Instant::now();
        let err = client.join().unwrap().unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(err.to_string(), "primary went silent");
        assert!(lock(&h.injector).is_idle());
        assert_eq!(
            h.events().last(),
            Some(&Recorded::Key(k(key::RIGHT_CTRL), false))
        );
    }

    #[test]
    fn sends_pings_and_display_changes() {
        let mut h = Harness::new();
        h.config.ping_interval = Duration::from_millis(50);
        let (mut primary, client) = h.start();
        welcome(&mut primary);

        assert!(matches!(
            read_message(&mut primary).unwrap(),
            Message::Ping(_)
        ));

        *h.host.displays.lock().unwrap() = vec![display(-2560), display(0)];
        h.host.generation.fetch_add(1, Ordering::SeqCst);
        let changed = loop {
            match read_message(&mut primary).unwrap() {
                Message::Ping(_) => continue,
                other => break other,
            }
        };
        assert_eq!(
            changed,
            Message::DisplaysChanged {
                displays: vec![display(-2560), display(0)]
            }
        );
        drop(primary);
        client.join().unwrap().unwrap_err();
    }

    #[test]
    fn unpaired_pc_is_refused_unless_the_mac_is_pairing() {
        let h = Harness::unpaired(|_| true);
        let client = h.spawn_client();
        assert!(h.accept().is_err());
        let err = client.join().unwrap().unwrap_err();
        assert!(
            matches!(
                err,
                ClientError::Transport(onemouse_transport::Error::NotPairing { here: false })
            ),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "the other device isn't in pairing mode");
    }

    #[test]
    fn pairs_on_first_connection_when_both_users_confirm() {
        let codes = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&codes);
        let mut h = Harness::unpaired(move |req| {
            seen.lock().unwrap().push(req.code.clone());
            true
        });
        h.mac.can_pair = true;
        let (mut primary, client) = h.start();
        welcome(&mut primary);
        send(&mut primary, &[Message::Ping(5)]);
        assert_eq!(read_message(&mut primary).unwrap(), Message::Pong(5));
        assert_eq!(codes.lock().unwrap().len(), 1);
        let pinned = h.config.security.trust.lock().unwrap().peers().to_vec();
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].name, "macbook");
        assert_eq!(&pinned[0].key, h.mac.identity.public_key());
        drop(primary);
        client.join().unwrap().unwrap_err();
    }

    fn found(name: &str, fp: &str, version: u16, ip: &str) -> discovery::Found {
        discovery::Found {
            name: name.into(),
            version: Some(version),
            fingerprint: fp.into(),
            addrs: vec![ip.parse().unwrap()],
            port: 24801,
        }
    }

    #[test]
    fn discovery_prefers_paired_macs_and_keeps_every_claimant() {
        let v = PROTOCOL_VERSION;
        let real = found("mac", "aa", v, "10.0.0.2");
        let spoof = found("mac", "aa", v, "10.0.0.66");
        let other = found("office", "bb", v, "10.0.0.3");
        let old = found("old mac", "cc", v - 1, "10.0.0.4");
        let all = [spoof.clone(), other.clone(), real.clone(), old.clone()];

        // Paired with "aa": both claimants, the handshake sorts them out.
        let chosen = choose(&all, &["aa".into()]).unwrap();
        assert_eq!(chosen, [&spoof, &real]);

        // First pairing, one fingerprint around (plus an old version).
        let first = [real.clone(), old.clone()];
        assert_eq!(choose(&first, &[]).unwrap(), [&real]);

        // First pairing, two different Macs: ask the user.
        let err = choose(&[real.clone(), other], &[]).unwrap_err();
        assert!(err.contains("pass --host"), "{err}");
        assert!(choose(&[], &[]).unwrap_err().contains("no Mac found"));
    }

    #[test]
    fn backoff_doubles_up_to_max_and_resets() {
        let mut b = Backoff::new(Duration::from_millis(500), Duration::from_secs(3));
        let delays: Vec<_> = (0..5).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(delays, [500, 1000, 2000, 3000, 3000]);
        b.reset();
        assert_eq!(b.next_delay().as_millis(), 500);
    }
}
