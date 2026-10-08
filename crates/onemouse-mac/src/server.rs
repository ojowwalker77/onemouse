//! TCP listener for the secondary: encrypted handshake (pinned keys or
//! pairing), then `Hello`, heartbeat and display updates. One secondary at a
//! time; a new connection replaces the old one so a PC that reconnects after
//! a network blip doesn't wait for the old one to time out.
//!
//! Sends never block the caller (the event tap): messages go through a
//! channel to a writer thread per connection.

use std::io::{self, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use onemouse_protocol::{
    Display, FrameError, Message, PROTOCOL_VERSION, read_message, write_message,
};
use onemouse_transport::{Identity, Options, PairingRequest, TrustStore};

use crate::log;

/// Asks the user whether a pairing code matches the PC's screen. Called on
/// a connection thread; may block until they answer.
pub type Confirm = dyn Fn(&PairingRequest) -> bool + Send + Sync;

/// Who may connect: this Mac's key, the PCs it has paired with, and whether
/// pairing a new one is allowed right now.
pub struct Security {
    pub identity: Identity,
    pub trust: Mutex<TrustStore>,
    pairing_until: Mutex<Option<Instant>>,
    confirm: Box<Confirm>,
}

impl Security {
    pub fn new(identity: Identity, trust: TrustStore, confirm: Box<Confirm>) -> Self {
        Self {
            identity,
            trust: Mutex::new(trust),
            pairing_until: Mutex::new(None),
            confirm,
        }
    }

    /// Lets an unknown PC pair during the next `duration`.
    pub fn open_pairing(&self, duration: Duration) {
        *self.pairing() = Some(Instant::now() + duration);
    }

    pub fn close_pairing(&self) {
        *self.pairing() = None;
    }

    /// Time left to pair, if pairing is open.
    pub fn pairing_left(&self) -> Option<Duration> {
        let until = (*self.pairing())?;
        until.checked_duration_since(Instant::now())
    }

    fn pairing(&self) -> MutexGuard<'_, Option<Instant>> {
        self.pairing_until.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for Security {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Security")
            .field("fingerprint", &self.identity.fingerprint())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Name sent in the handshake and `Welcome`.
    pub name: String,
    pub security: Arc<Security>,
    /// Ping when nothing else was sent for this long.
    pub ping_interval: Duration,
    /// Drop the connection when nothing arrives for this long.
    pub silence_timeout: Duration,
}

impl Config {
    pub fn new(name: impl Into<String>, security: Arc<Security>) -> Self {
        Self {
            name: name.into(),
            security,
            ping_interval: Duration::from_secs(2),
            silence_timeout: Duration::from_secs(6),
        }
    }
}

/// The connected secondary.
#[derive(Debug)]
pub struct Peer {
    pub name: String,
    /// Remote address, for logs.
    pub addr: String,
    pub displays: Vec<Display>,
    id: u64,
    tx: Sender<Message>,
    stream: TcpStream,
}

impl Peer {
    /// Unique per connection.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Queues `msg`; never blocks. Lost if the connection is going away.
    pub fn send(&self, msg: Message) {
        let _ = self.tx.send(msg);
    }
}

/// Shared between the event tap and the connection threads.
#[derive(Debug, Default)]
pub struct Link {
    peer: Mutex<Option<Peer>>,
    next_id: AtomicU64,
}

impl Link {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Runs `f` with the current peer, holding the lock so the peer can't
    /// change halfway through.
    pub fn with_peer<R>(&self, f: impl FnOnce(Option<&Peer>) -> R) -> R {
        f(self.lock().as_ref())
    }

    fn lock(&self) -> MutexGuard<'_, Option<Peer>> {
        self.peer.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn attach(&self, mut peer: Peer) -> u64 {
        peer.id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = peer.id;
        if let Some(old) = self.lock().replace(peer) {
            log!("{} replaced by a new connection", old.name);
            let _ = old.stream.shutdown(Shutdown::Both);
        }
        id
    }

    fn detach(&self, id: u64) {
        let mut peer = self.lock();
        if peer.as_ref().is_some_and(|p| p.id == id) {
            *peer = None;
        }
    }

    fn set_displays(&self, id: u64, displays: Vec<Display>) {
        if let Some(peer) = self.lock().as_mut().filter(|p| p.id == id) {
            peer.displays = displays;
        }
    }
}

/// Accepts secondaries on `listener` forever, on a background thread.
pub fn serve(listener: TcpListener, link: Arc<Link>, config: Config) -> JoinHandle<()> {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    log!("accept failed: {e}");
                    continue;
                }
            };
            let (link, config) = (Arc::clone(&link), config.clone());
            thread::spawn(move || {
                let addr = stream
                    .peer_addr()
                    .map_or_else(|_| "?".into(), |a| a.to_string());
                match session(stream, &link, &config) {
                    Ok(name) => log!("{name} ({addr}) disconnected"),
                    Err(e) => log!("connection from {addr} ended: {e}"),
                }
            });
        }
    })
}

#[derive(Debug)]
pub enum SessionError {
    Io(io::Error),
    Transport(onemouse_transport::Error),
    Frame(FrameError),
    Protocol(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Transport(e) => write!(f, "{e}"),
            Self::Frame(FrameError::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                write!(f, "secondary went silent")
            }
            Self::Frame(e) => write!(f, "{e}"),
            Self::Protocol(what) => write!(f, "protocol error: {what}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<io::Error> for SessionError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<FrameError> for SessionError {
    fn from(e: FrameError) -> Self {
        Self::Frame(e)
    }
}

impl From<onemouse_transport::Error> for SessionError {
    fn from(e: onemouse_transport::Error) -> Self {
        Self::Transport(e)
    }
}

/// One connection, from the handshake until it drops. Returns the peer's
/// name.
fn session(stream: TcpStream, link: &Link, config: &Config) -> Result<String, SessionError> {
    stream.set_nodelay(true)?;
    let addr = stream
        .peer_addr()
        .map_or_else(|_| "?".into(), |a| a.ip().to_string());

    let security = &config.security;
    let opts = Options {
        can_pair: security.pairing_left().is_some(),
        confirm: &*security.confirm,
        ..Options::new(&security.identity, &config.name, &security.trust)
    };
    let (secure, who) = onemouse_transport::accept(stream, &opts)?;
    if who.newly_paired {
        log!("paired with {} ({})", who.name, who.fingerprint());
        // One pairing per "Pair a New PC…".
        security.close_pairing();
    }
    secure.set_read_timeout(Some(config.silence_timeout))?;
    let tcp = secure.tcp().try_clone()?;
    let (mut reader, mut writer) = secure.split();

    let hello = match read_message(&mut reader)? {
        Message::Hello(hello) => hello,
        other => {
            return Err(SessionError::Protocol(format!(
                "expected Hello, got {other:?}"
            )));
        }
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        let reason = format!(
            "protocol version {} not supported, this Mac speaks {PROTOCOL_VERSION}",
            hello.protocol_version
        );
        write_message(
            &mut writer,
            &Message::Reject {
                reason: reason.clone(),
            },
        )?;
        return Err(SessionError::Protocol(reason));
    }
    write_message(
        &mut writer,
        &Message::Welcome {
            protocol_version: PROTOCOL_VERSION,
            name: config.name.clone(),
        },
    )?;

    // The name the PC proved with its key, not whatever `Hello` claims.
    let name = who.name;
    log!(
        "{name} connected with {} display(s): {}",
        hello.displays.len(),
        describe(&hello.displays)
    );
    let (tx, rx) = mpsc::channel();
    let id = link.attach(Peer {
        name: name.clone(),
        addr,
        displays: hello.displays,
        id: 0,
        tx: tx.clone(),
        stream: tcp.try_clone()?,
    });
    let ping_interval = config.ping_interval;
    let shutdown = tcp.try_clone()?;
    let writer_thread = thread::spawn(move || write_loop(writer, shutdown, rx, ping_interval));

    let result = loop {
        match read_message(&mut reader) {
            Ok(Message::DisplaysChanged { displays }) => {
                log!("{name} displays changed: {}", describe(&displays));
                link.set_displays(id, displays);
            }
            Ok(Message::Ping(n)) => {
                let _ = tx.send(Message::Pong(n));
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Reject { reason }) => {
                break Err(SessionError::Protocol(format!("rejected: {reason}")));
            }
            Ok(other) => log!("ignoring unexpected {other:?} from {name}"),
            Err(e) => break Err(e.into()),
        }
    };

    link.detach(id);
    drop(tx);
    let _ = tcp.shutdown(Shutdown::Both);
    let _ = writer_thread.join();
    match result {
        Err(SessionError::Frame(FrameError::Io(e))) if e.kind() == io::ErrorKind::UnexpectedEof => {
            Ok(name)
        }
        other => other.map(|()| name),
    }
}

/// Sends queued messages, pinging when idle. `tcp` is the same connection,
/// to wake the reader if writing fails.
fn write_loop(
    mut writer: impl Write,
    tcp: TcpStream,
    rx: Receiver<Message>,
    ping_interval: Duration,
) {
    let mut ping = 0u64;
    loop {
        let msg = match rx.recv_timeout(ping_interval) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => {
                ping += 1;
                Message::Ping(ping)
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if write_message(&mut writer, &msg).is_err() {
            let _ = tcp.shutdown(Shutdown::Both);
            return;
        }
    }
}

fn describe(displays: &[Display]) -> String {
    displays
        .iter()
        .map(|d| {
            format!(
                "{}x{} at ({}, {}) @{}",
                d.width, d.height, d.x, d.y, d.scale
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use onemouse_protocol::{Hello, Os};
    use onemouse_transport::SecureStream;

    fn display(x: i32) -> Display {
        Display {
            id: 1,
            x,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.25,
            primary: true,
        }
    }

    /// The Mac's security, with `pc` already paired.
    fn security(pc: &Identity) -> Arc<Security> {
        let mut trust = TrustStore::in_memory();
        trust.pin("pc", pc.public_key()).unwrap();
        Arc::new(Security::new(
            Identity::generate().unwrap(),
            trust,
            Box::new(|_| true),
        ))
    }

    struct Pc {
        identity: Identity,
        trust: Mutex<TrustStore>,
    }

    impl Pc {
        fn new() -> Self {
            Self {
                identity: Identity::generate().unwrap(),
                trust: Mutex::new(TrustStore::in_memory()),
            }
        }

        /// Pins the Mac, as a finished pairing would have.
        fn trusting(self, mac: &Security) -> Self {
            self.trust
                .lock()
                .unwrap()
                .pin("mac", mac.identity.public_key())
                .unwrap();
            self
        }

        fn connect(
            &self,
            port: u16,
            can_pair: bool,
        ) -> Result<SecureStream, onemouse_transport::Error> {
            let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let opts = Options {
                can_pair,
                confirm: &|_| true,
                ..Options::new(&self.identity, "pc", &self.trust)
            };
            let (mut s, _) = onemouse_transport::connect(tcp, &opts)?;
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            hello(&mut s, PROTOCOL_VERSION);
            Ok(s)
        }
    }

    fn hello(s: &mut SecureStream, version: u16) {
        write_message(
            s,
            &Message::Hello(Hello {
                protocol_version: version,
                name: "pc".into(),
                os: Os::Windows,
                displays: vec![display(0)],
            }),
        )
        .unwrap();
    }

    fn start(config: Config) -> (Arc<Link>, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let link = Link::new();
        serve(listener, Arc::clone(&link), config);
        (link, port)
    }

    /// A paired PC and the Mac it trusts, listening.
    fn paired() -> (Pc, Arc<Link>, u16) {
        let pc = Pc::new();
        let security = security(&pc.identity);
        let pc = pc.trusting(&security);
        let (link, port) = start(Config::new("mac", security));
        (pc, link, port)
    }

    /// Polls until `f` holds, for state changed by other threads.
    fn eventually(f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "condition never became true");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn peer_displays(link: &Link) -> Option<Vec<Display>> {
        link.with_peer(|p| p.map(|p| p.displays.clone()))
    }

    #[test]
    fn welcomes_tracks_displays_and_forwards_messages() {
        let (pc, link, port) = paired();
        let mut s = pc.connect(port, false).unwrap();
        assert_eq!(
            read_message(&mut s).unwrap(),
            Message::Welcome {
                protocol_version: PROTOCOL_VERSION,
                name: "mac".into()
            }
        );
        eventually(|| peer_displays(&link) == Some(vec![display(0)]));

        write_message(
            &mut s,
            &Message::DisplaysChanged {
                displays: vec![display(-1920)],
            },
        )
        .unwrap();
        eventually(|| peer_displays(&link) == Some(vec![display(-1920)]));

        link.with_peer(|p| p.unwrap().send(Message::Enter { x: 1, y: 2 }));
        assert_eq!(read_message(&mut s).unwrap(), Message::Enter { x: 1, y: 2 });

        write_message(&mut s, &Message::Ping(7)).unwrap();
        assert_eq!(read_message(&mut s).unwrap(), Message::Pong(7));

        drop(s);
        eventually(|| peer_displays(&link).is_none());
    }

    #[test]
    fn rejects_other_versions() {
        let (pc, link, port) = paired();
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let opts = Options::new(&pc.identity, "pc", &pc.trust);
        let (mut s, _) = onemouse_transport::connect(tcp, &opts).unwrap();
        hello(&mut s, PROTOCOL_VERSION + 1);
        assert!(matches!(
            read_message(&mut s).unwrap(),
            Message::Reject { .. }
        ));
        assert!(peer_displays(&link).is_none());
    }

    #[test]
    fn unknown_pcs_need_pairing_mode() {
        let mac = Arc::new(Security::new(
            Identity::generate().unwrap(),
            TrustStore::in_memory(),
            Box::new(|req| req.code.len() > 1),
        ));
        let (link, port) = start(Config::new("mac", Arc::clone(&mac)));
        let stranger = Pc::new();
        assert!(stranger.connect(port, true).is_err());
        assert!(peer_displays(&link).is_none());

        mac.open_pairing(Duration::from_secs(60));
        let mut s = stranger.connect(port, true).unwrap();
        read_message(&mut s).unwrap();
        eventually(|| peer_displays(&link).is_some());
        // Used up: the next stranger can't pair without reopening.
        assert!(mac.pairing_left().is_none());
        assert!(Pc::new().connect(port, true).is_err());
    }

    #[test]
    fn newest_connection_wins() {
        let (pc, link, port) = paired();
        let mut old = pc.connect(port, false).unwrap();
        read_message(&mut old).unwrap();
        eventually(|| peer_displays(&link).is_some());

        let mut new = pc.connect(port, false).unwrap();
        read_message(&mut new).unwrap();
        // The old connection gets closed.
        let mut closed = false;
        for _ in 0..10 {
            match read_message(&mut old) {
                Ok(Message::Ping(_)) => continue,
                _ => {
                    closed = true;
                    break;
                }
            }
        }
        assert!(closed);
        link.with_peer(|p| p.unwrap().send(Message::Leave));
        assert_eq!(read_message(&mut new).unwrap(), Message::Leave);
    }

    #[test]
    fn pings_when_idle_and_drops_silent_peers() {
        let pc = Pc::new();
        let security = security(&pc.identity);
        let pc = pc.trusting(&security);
        let mut config = Config::new("mac", security);
        config.ping_interval = Duration::from_millis(50);
        config.silence_timeout = Duration::from_millis(300);
        let (link, port) = start(config);
        let mut s = pc.connect(port, false).unwrap();
        read_message(&mut s).unwrap();
        assert!(matches!(read_message(&mut s).unwrap(), Message::Ping(_)));
        // We never answer or ping, so the Mac drops us.
        eventually(|| peer_displays(&link).is_none());
    }
}
