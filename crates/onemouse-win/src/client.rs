//! Connection to the primary: handshake, apply incoming messages, heartbeat,
//! display updates, reconnect. Portable (tested on every CI runner); the
//! Windows specifics come in through [`Host`] and the injector [`Backend`].

use std::fmt;
use std::io;
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use onemouse_protocol::{
    Display, FrameError, Hello, Message, Os, PROTOCOL_VERSION, read_message, write_message,
};

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

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub connect_timeout: Duration,
    pub ping_interval: Duration,
    /// Drop the connection when nothing arrives for this long.
    pub silence_timeout: Duration,
    /// How often the writer checks for display changes and pings.
    pub poll_interval: Duration,
}

impl Config {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
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
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
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

/// Connects forever, reconnecting with backoff. Everything held is released
/// whenever a session ends, however it ends.
pub fn run<H: Host, B: Backend + Send + 'static>(
    config: &Config,
    host: Arc<H>,
    injector: SharedInjector<B>,
) -> ! {
    let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(30));
    loop {
        let started = Instant::now();
        let mut welcomed = false;
        match run_session(config, &host, &injector, || welcomed = true) {
            Ok(()) => log!("disconnected"),
            Err(e) => log!("connection to {}:{} ended: {e}", config.host, config.port),
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
    on_welcome: impl FnOnce(),
) -> Result<(), ClientError> {
    let mut stream = connect(config)?;
    let result = session(config, host, injector, &mut stream, on_welcome);
    lock(injector).release_all();
    let _ = stream.shutdown(Shutdown::Both);
    result
}

fn connect(config: &Config) -> Result<TcpStream, ClientError> {
    let addrs: Vec<SocketAddr> = (config.host.as_str(), config.port)
        .to_socket_addrs()?
        .collect();
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "host resolved to no address");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, config.connect_timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = e,
        }
    }
    Err(last_err.into())
}

fn session<H: Host, B: Backend + Send + 'static>(
    config: &Config,
    host: &Arc<H>,
    injector: &SharedInjector<B>,
    stream: &mut TcpStream,
    on_welcome: impl FnOnce(),
) -> Result<(), ClientError> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(config.silence_timeout))?;

    let generation = host.display_generation();
    let displays = host.displays();
    write_message(
        stream,
        &Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            name: host.name(),
            os: Os::Windows,
            displays: displays.clone(),
        }),
    )?;

    match read_message(stream)? {
        Message::Welcome {
            protocol_version,
            name,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                return Err(ClientError::Protocol(format!(
                    "primary speaks v{protocol_version}, we speak v{PROTOCOL_VERSION}"
                )));
            }
            log!(
                "connected to {name} ({} display(s) reported)",
                displays.len()
            );
        }
        Message::Reject { reason } => return Err(ClientError::Rejected(reason)),
        other => {
            return Err(ClientError::Protocol(format!(
                "expected Welcome, got {other:?}"
            )));
        }
    }
    on_welcome();

    let (tx, rx) = mpsc::channel();
    let writer = {
        let stream = stream.try_clone()?;
        let host = Arc::clone(host);
        let config = config.clone();
        thread::Builder::new()
            .name("onemouse-writer".into())
            .spawn(move || writer(stream, rx, &*host, &config, generation, displays))?
    };

    let result = read_loop(stream, injector, &tx);

    // Stop the writer: closing the channel ends its loop, shutting the socket
    // down unblocks a stuck write.
    drop(tx);
    let _ = stream.shutdown(Shutdown::Both);
    let _ = writer.join();
    result
}

fn read_loop<B: Backend>(
    stream: &mut TcpStream,
    injector: &SharedInjector<B>,
    tx: &Sender<Message>,
) -> Result<(), ClientError> {
    let mut session = Session::default();
    loop {
        let msg = read_message(stream)?;
        let reply = session.handle(msg, &mut lock(injector))?;
        if let Some(reply) = reply
            && tx.send(reply).is_err()
        {
            return Err(ClientError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "writer stopped",
            )));
        }
    }
}

/// Sends replies, pings and display updates. The only thread writing to the
/// socket after the handshake.
fn writer<H: Host + ?Sized>(
    mut stream: TcpStream,
    rx: mpsc::Receiver<Message>,
    host: &H,
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
                    displays = now;
                    write_message(
                        &mut stream,
                        &Message::DisplaysChanged {
                            displays: displays.clone(),
                        },
                    )?;
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
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// Applies primary → secondary messages to the injector.
#[derive(Debug, Default)]
pub struct Session {
    /// Whether the cursor is on this machine (between `Enter` and `Leave`).
    /// Input outside that window is ignored.
    entered: bool,
}

impl Session {
    /// Returns the reply to send, if any. Injection failures (e.g. UIPI
    /// blocking input to an elevated window) are logged, not fatal.
    pub fn handle<B: Backend>(
        &mut self,
        msg: Message,
        inj: &mut Injector<B>,
    ) -> Result<Option<Message>, ClientError> {
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
            Message::Ping(id) => return Ok(Some(Message::Pong(id))),
            Message::Pong(_) => Ok(()),
            Message::DisplaysChanged { .. } => {
                log!("ignoring DisplaysChanged from the primary");
                Ok(())
            }
            Message::Reject { reason } => return Err(ClientError::Rejected(reason)),
            Message::Hello(_) | Message::Welcome { .. } => {
                return Err(ClientError::Protocol(format!("unexpected {msg:?}")));
            }
        };
        if let Err(e) = result {
            log!("injection failed: {e}");
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::{Recorded, RecordingBackend};
    use crate::keymap;
    use onemouse_protocol::{KeyCode, MouseButton, key};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestHost {
        displays: Mutex<Vec<Display>>,
        generation: AtomicU64,
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

    struct Harness {
        listener: TcpListener,
        config: Config,
        host: Arc<TestHost>,
        injector: SharedInjector<RecordingBackend>,
    }

    impl Harness {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut config = Config::new("127.0.0.1", listener.local_addr().unwrap().port());
            config.ping_interval = Duration::from_secs(60);
            config.poll_interval = Duration::from_millis(10);
            Self {
                listener,
                config,
                host: Arc::new(TestHost {
                    displays: Mutex::new(vec![display(0)]),
                    generation: AtomicU64::new(0),
                }),
                injector: Arc::new(Mutex::new(Injector::new(RecordingBackend::default()))),
            }
        }

        /// Runs a session in the background, returns the primary's end of the
        /// socket after the handshake.
        fn start(&self) -> (TcpStream, thread::JoinHandle<Result<(), ClientError>>) {
            let (config, host, injector) = (
                self.config.clone(),
                Arc::clone(&self.host),
                Arc::clone(&self.injector),
            );
            let client = thread::spawn(move || run_session(&config, &host, &injector, || {}));
            let (mut primary, _) = self.listener.accept().unwrap();
            primary
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
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

    fn welcome(primary: &mut TcpStream) {
        write_message(
            primary,
            &Message::Welcome {
                protocol_version: PROTOCOL_VERSION,
                name: "macbook".into(),
            },
        )
        .unwrap();
    }

    fn send(primary: &mut TcpStream, msgs: &[Message]) {
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
            }],
        );
        let err = client.join().unwrap().unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)), "{err:?}");
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
    fn backoff_doubles_up_to_max_and_resets() {
        let mut b = Backoff::new(Duration::from_millis(500), Duration::from_secs(3));
        let delays: Vec<_> = (0..5).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(delays, [500, 1000, 2000, 3000, 3000]);
        b.reset();
        assert_eq!(b.next_delay().as_millis(), 500);
    }
}
