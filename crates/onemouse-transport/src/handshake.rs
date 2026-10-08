//! Noise XX handshake, trust exchange and pairing. The wire format is
//! documented at the crate root.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use snow::HandshakeState;

use crate::identity::{Identity, PublicKey, fingerprint};
use crate::secure::{SecureStream, halves};
use crate::trust::{Trust, TrustStore};
use crate::{NOISE_PARAMS, PROLOGUE};

/// Longest device name carried in the handshake.
pub const MAX_NAME_LEN: usize = 255;

/// Shown to the user when a new device wants to pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRequest {
    pub peer_name: String,
    pub peer_fingerprint: String,
    /// Six digits, e.g. `"042 917"`. Same on both devices unless someone is
    /// in the middle.
    pub code: String,
}

/// The authenticated peer of an established connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub key: PublicKey,
    /// Whether it was paired during this handshake.
    pub newly_paired: bool,
}

impl Peer {
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.key)
    }
}

pub struct Options<'a> {
    pub identity: &'a Identity,
    /// This device's name, sent to the peer.
    pub name: &'a str,
    pub trust: &'a Mutex<TrustStore>,
    /// Whether this side accepts pairing with unknown devices right now (on
    /// the primary: only while the user has pairing mode open).
    pub can_pair: bool,
    /// Asks the user whether the code matches the other screen. Called on
    /// the handshake's thread; may block until the user answers.
    pub confirm: &'a (dyn Fn(&PairingRequest) -> bool + Sync),
    /// Limit for the Noise handshake and trust exchange.
    pub handshake_timeout: Duration,
    /// Limit for both users to confirm the code.
    pub pairing_timeout: Duration,
}

impl<'a> Options<'a> {
    /// No pairing; only already-pinned peers get in.
    pub fn new(identity: &'a Identity, name: &'a str, trust: &'a Mutex<TrustStore>) -> Self {
        Self {
            identity,
            name,
            trust,
            can_pair: false,
            confirm: &|_| false,
            handshake_timeout: Duration::from_secs(10),
            pairing_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Noise(snow::Error),
    Protocol(String),
    /// We have `name` pinned to a different key. Possibly an impostor;
    /// the user must forget the old pairing deliberately.
    KeyChanged {
        name: String,
    },
    /// The peer has our name pinned to a different key (it was paired with
    /// another install of this device). It must forget us first.
    PeerKeyChanged {
        name: String,
    },
    /// Pairing is needed but not allowed: here (`here == true`) or on the
    /// peer, e.g. the Mac isn't in pairing mode.
    NotPairing {
        here: bool,
    },
    /// A user said the codes don't match (or didn't answer in time).
    Declined {
        by_peer: bool,
    },
    /// The peer didn't confirm within the pairing timeout.
    PairingTimeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Noise(e) => write!(f, "handshake failed: {e}"),
            Self::Protocol(what) => write!(f, "protocol error: {what}"),
            Self::KeyChanged { name } => write!(
                f,
                "{name} presented a different key than the one paired with this device. \
                 If you reinstalled or reset it, forget the old pairing and pair again; \
                 otherwise someone may be impersonating it"
            ),
            Self::PeerKeyChanged { name } => write!(
                f,
                "{name} has this device paired under a different key; forget it there and pair again"
            ),
            Self::NotPairing { here: true } => write!(f, "unknown device and pairing is off here"),
            Self::NotPairing { here: false } => {
                write!(f, "the other device isn't in pairing mode")
            }
            Self::Declined { by_peer: false } => write!(f, "pairing declined here"),
            Self::Declined { by_peer: true } => write!(f, "pairing declined on the other device"),
            Self::PairingTimeout => write!(f, "pairing timed out"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<snow::Error> for Error {
    fn from(e: snow::Error) -> Self {
        Self::Noise(e)
    }
}

/// Secondary side: we initiate.
pub fn connect(stream: TcpStream, opts: &Options) -> Result<(SecureStream, Peer), Error> {
    establish(stream, opts, true)
}

/// Primary side: we respond.
pub fn accept(stream: TcpStream, opts: &Options) -> Result<(SecureStream, Peer), Error> {
    establish(stream, opts, false)
}

fn establish(
    stream: TcpStream,
    opts: &Options,
    initiator: bool,
) -> Result<(SecureStream, Peer), Error> {
    let previous_timeout = stream.read_timeout()?;
    stream.set_read_timeout(Some(opts.handshake_timeout))?;
    let result = establish_inner(stream.try_clone()?, opts, initiator);
    // Best effort: macOS returns EINVAL for setsockopt once the peer has
    // closed, and the handshake's own result matters more.
    let _ = stream.set_read_timeout(previous_timeout);
    result
}

fn establish_inner(
    mut stream: TcpStream,
    opts: &Options,
    initiator: bool,
) -> Result<(SecureStream, Peer), Error> {
    let name = opts.name.as_bytes();
    let name = &name[..floor_char_boundary(opts.name, MAX_NAME_LEN.min(name.len()))];
    let builder = snow::Builder::new(NOISE_PARAMS.parse()?)
        .prologue(PROLOGUE)?
        .local_private_key(opts.identity.private_key())?;

    // XX: -> e; <- e, ee, s, es [name]; -> s, se [name]
    let (hs, peer_name) = if initiator {
        let mut hs = builder.build_initiator()?;
        send_handshake(&mut stream, &mut hs, &[])?;
        let peer_name = recv_handshake(&mut stream, &mut hs)?;
        send_handshake(&mut stream, &mut hs, name)?;
        (hs, peer_name)
    } else {
        let mut hs = builder.build_responder()?;
        recv_handshake(&mut stream, &mut hs)?;
        send_handshake(&mut stream, &mut hs, name)?;
        let peer_name = recv_handshake(&mut stream, &mut hs)?;
        (hs, peer_name)
    };
    let peer_name = String::from_utf8(peer_name)
        .map_err(|_| Error::Protocol("peer name is not UTF-8".into()))?;
    let key: PublicKey = hs
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| Error::Protocol("no remote static key".into()))?;
    let code = pairing_code(hs.get_handshake_hash());
    let state = hs.into_stateless_transport_mode()?;
    let (reader, writer) = halves(state, stream.try_clone()?, stream.try_clone()?);
    let mut secure = SecureStream { reader, writer };

    let newly_paired = exchange_trust(&mut secure, &stream, opts, &peer_name, &key, code)?;
    Ok((
        secure,
        Peer {
            name: peer_name,
            key,
            newly_paired,
        },
    ))
}

const TRUST_PINNED: u8 = 0;
const TRUST_UNKNOWN: u8 = 1;
const TRUST_KEY_CHANGED: u8 = 2;

/// Each side sends `[trust, can_pair]`, then both resolve the same way.
/// Returns whether the peer was paired (pinned) just now.
///
/// The peer's record is advisory: it can make us stricter (fail on its key
/// conflict, or pair because it doesn't know us) but never looser. Whether we
/// trust the peer depends only on our own store, and an unknown peer always
/// needs our own user's confirmation.
fn exchange_trust(
    secure: &mut SecureStream,
    tcp: &TcpStream,
    opts: &Options,
    peer_name: &str,
    key: &PublicKey,
    code: String,
) -> Result<bool, Error> {
    let trust = opts
        .trust
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .check(peer_name, key);
    let ours = match trust {
        Trust::Pinned => TRUST_PINNED,
        Trust::Unknown => TRUST_UNKNOWN,
        Trust::KeyChanged => TRUST_KEY_CHANGED,
    };
    secure.writer.write_record(&[ours, opts.can_pair.into()])?;
    let theirs = read_record(secure)?;
    let &[theirs, peer_can_pair] = theirs.as_slice() else {
        return Err(Error::Protocol("malformed trust record".into()));
    };
    if theirs > TRUST_KEY_CHANGED || peer_can_pair > 1 {
        return Err(Error::Protocol("malformed trust record".into()));
    }

    if ours == TRUST_KEY_CHANGED {
        return Err(Error::KeyChanged {
            name: peer_name.into(),
        });
    }
    if theirs == TRUST_KEY_CHANGED {
        return Err(Error::PeerKeyChanged {
            name: peer_name.into(),
        });
    }
    if ours == TRUST_PINNED && theirs == TRUST_PINNED {
        // Known key: follow a rename, never a key change.
        let mut store = opts.trust.lock().unwrap_or_else(|e| e.into_inner());
        if !store
            .peers()
            .iter()
            .any(|p| &p.key == key && p.name == peer_name)
        {
            let _ = store.pin(peer_name, key);
        }
        return Ok(false);
    }
    if !opts.can_pair {
        return Err(Error::NotPairing { here: true });
    }
    if peer_can_pair == 0 {
        return Err(Error::NotPairing { here: false });
    }

    // Pairing: both users compare the code, each side sends one record.
    let started = Instant::now();
    let request = PairingRequest {
        peer_name: peer_name.into(),
        peer_fingerprint: fingerprint(key),
        code,
    };
    let accepted = (opts.confirm)(&request) && started.elapsed() < opts.pairing_timeout;
    secure.writer.write_record(&[accepted.into()])?;
    if !accepted {
        return Err(Error::Declined { by_peer: false });
    }
    let remaining = opts
        .pairing_timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(1));
    // Best effort (see `establish`): if the peer already closed, the read
    // below reports it, e.g. after its `[0]`.
    let _ = tcp.set_read_timeout(Some(remaining));
    let answer = match read_record(secure) {
        Ok(answer) => answer,
        Err(Error::Io(e))
            if matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            return Err(Error::PairingTimeout);
        }
        Err(e) => return Err(e),
    };
    match answer.as_slice() {
        [1] => {}
        [0] => return Err(Error::Declined { by_peer: true }),
        _ => return Err(Error::Protocol("malformed pairing record".into())),
    }
    opts.trust
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pin(peer_name, key)?;
    Ok(true)
}

fn read_record(secure: &mut SecureStream) -> Result<Vec<u8>, Error> {
    match secure.reader.read_record()? {
        Some(record) => Ok(record.to_vec()),
        None => Err(Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed the connection during the handshake",
        ))),
    }
}

/// Six decimal digits from the handshake hash, which both sides only share
/// if nobody substituted keys in the middle.
pub fn pairing_code(handshake_hash: &[u8]) -> String {
    let n = u64::from_be_bytes(handshake_hash[..8].try_into().expect("32-byte hash")) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

fn send_handshake(
    stream: &mut TcpStream,
    hs: &mut HandshakeState,
    payload: &[u8],
) -> Result<(), Error> {
    let mut buf = vec![0; 2 + 65535];
    let n = hs.write_message(payload, &mut buf[2..])?;
    buf[..2].copy_from_slice(&(n as u16).to_be_bytes());
    stream.write_all(&buf[..2 + n])?;
    Ok(())
}

fn recv_handshake(stream: &mut TcpStream, hs: &mut HandshakeState) -> Result<Vec<u8>, Error> {
    let mut len = [0; 2];
    stream.read_exact(&mut len)?;
    let mut msg = vec![0; u16::from_be_bytes(len) as usize];
    stream.read_exact(&mut msg)?;
    let mut payload = vec![0; msg.len()];
    let n = hs.read_message(&msg, &mut payload)?;
    payload.truncate(n);
    if payload.len() > MAX_NAME_LEN {
        return Err(Error::Protocol("handshake payload too long".into()));
    }
    Ok(payload)
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
