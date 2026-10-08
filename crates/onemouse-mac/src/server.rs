//! TCP listener for the secondary: handshake, heartbeat, display updates.
//! One secondary at a time; a new connection replaces the old one so a PC
//! that reconnects after a network blip doesn't wait for the old one to time
//! out.
//!
//! Sends never block the caller (the event tap): messages go through a
//! channel to a writer thread per connection.

use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use onemouse_protocol::{
    Display, FrameError, Message, PROTOCOL_VERSION, read_message, write_message,
};

use crate::log;

#[derive(Debug, Clone)]
pub struct Config {
    /// Name sent in `Welcome`.
    pub name: String,
    /// Ping when nothing else was sent for this long.
    pub ping_interval: Duration,
    /// Drop the connection when nothing arrives for this long.
    pub silence_timeout: Duration,
}

impl Config {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ping_interval: Duration::from_secs(2),
            silence_timeout: Duration::from_secs(6),
        }
    }
}

/// The connected secondary.
#[derive(Debug)]
pub struct Peer {
    pub name: String,
    pub displays: Vec<Display>,
    id: u64,
    tx: Sender<Message>,
    stream: TcpStream,
}

impl Peer {
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
    Frame(FrameError),
    Protocol(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
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

/// One connection, from `Hello` until it drops. Returns the peer's name.
fn session(stream: TcpStream, link: &Link, config: &Config) -> Result<String, SessionError> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(config.silence_timeout))?;
    let mut reader = stream.try_clone()?;
    let mut writer = stream.try_clone()?;

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

    let name = hello.name;
    log!(
        "{name} connected with {} display(s): {}",
        hello.displays.len(),
        describe(&hello.displays)
    );
    let (tx, rx) = mpsc::channel();
    let id = link.attach(Peer {
        name: name.clone(),
        displays: hello.displays,
        id: 0,
        tx: tx.clone(),
        stream: stream.try_clone()?,
    });
    let ping_interval = config.ping_interval;
    let writer_thread = thread::spawn(move || write_loop(writer, rx, ping_interval));

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
    let _ = stream.shutdown(Shutdown::Both);
    let _ = writer_thread.join();
    match result {
        Err(SessionError::Frame(FrameError::Io(e))) if e.kind() == io::ErrorKind::UnexpectedEof => {
            Ok(name)
        }
        other => other.map(|()| name),
    }
}

fn write_loop(mut stream: TcpStream, rx: Receiver<Message>, ping_interval: Duration) {
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
        if write_message(&mut stream, &msg).is_err() {
            // Wakes the reader, which ends the session.
            let _ = stream.shutdown(Shutdown::Both);
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
    use std::time::Instant;

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

    fn start(config: Config) -> (Arc<Link>, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let link = Link::new();
        serve(listener, Arc::clone(&link), config);
        (link, port)
    }

    fn connect(port: u16, version: u16) -> TcpStream {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write_message(
            &mut s,
            &Message::Hello(Hello {
                protocol_version: version,
                name: "pc".into(),
                os: Os::Windows,
                displays: vec![display(0)],
            }),
        )
        .unwrap();
        s
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
        let (link, port) = start(Config::new("mac"));
        let mut pc = connect(port, PROTOCOL_VERSION);
        assert_eq!(
            read_message(&mut pc).unwrap(),
            Message::Welcome {
                protocol_version: PROTOCOL_VERSION,
                name: "mac".into()
            }
        );
        eventually(|| peer_displays(&link) == Some(vec![display(0)]));

        write_message(
            &mut pc,
            &Message::DisplaysChanged {
                displays: vec![display(-1920)],
            },
        )
        .unwrap();
        eventually(|| peer_displays(&link) == Some(vec![display(-1920)]));

        link.with_peer(|p| p.unwrap().send(Message::Enter { x: 1, y: 2 }));
        assert_eq!(
            read_message(&mut pc).unwrap(),
            Message::Enter { x: 1, y: 2 }
        );

        write_message(&mut pc, &Message::Ping(7)).unwrap();
        assert_eq!(read_message(&mut pc).unwrap(), Message::Pong(7));

        drop(pc);
        eventually(|| peer_displays(&link).is_none());
    }

    #[test]
    fn rejects_other_versions() {
        let (link, port) = start(Config::new("mac"));
        let mut pc = connect(port, PROTOCOL_VERSION + 1);
        assert!(matches!(
            read_message(&mut pc).unwrap(),
            Message::Reject { .. }
        ));
        assert!(peer_displays(&link).is_none());
    }

    #[test]
    fn newest_connection_wins() {
        let (link, port) = start(Config::new("mac"));
        let mut old = connect(port, PROTOCOL_VERSION);
        read_message(&mut old).unwrap();
        eventually(|| peer_displays(&link).is_some());

        let mut new = connect(port, PROTOCOL_VERSION);
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
        let mut config = Config::new("mac");
        config.ping_interval = Duration::from_millis(50);
        config.silence_timeout = Duration::from_millis(300);
        let (link, port) = start(config);
        let mut pc = connect(port, PROTOCOL_VERSION);
        read_message(&mut pc).unwrap();
        assert!(matches!(read_message(&mut pc).unwrap(), Message::Ping(_)));
        // We never answer or ping, so the Mac drops us.
        eventually(|| peer_displays(&link).is_none());
    }
}
