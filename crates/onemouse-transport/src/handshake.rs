//! Noise XX handshake, trust exchange and pairing. The wire format is
//! documented at the crate root.

use std::cell::Cell;
use std::fmt;
use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use blake2::{Blake2s256, Digest};
use snow::HandshakeState;
use subtle::ConstantTimeEq;

use crate::identity::{Identity, PublicKey, fingerprint};
use crate::secure::{SecureStream, halves, read_exact_by};
use crate::trust::{Trust, TrustStore, sanitize_name};
use crate::{NOISE_PARAMS, PROLOGUE};

/// Longest device name carried in the handshake.
pub const MAX_NAME_LEN: usize = 255;

/// Shown to the user when a new device wants to pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRequest {
    /// Sanitized: no control characters or bidi overrides.
    pub peer_name: String,
    pub peer_fingerprint: String,
    /// Six digits, e.g. `"042 917"`. Same on both devices unless someone is
    /// in the middle.
    pub code: String,
    /// After this the pairing fails anyway: close the dialog.
    pub deadline: Instant,
}

/// The authenticated peer of an established connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Sanitized, like in [`PairingRequest`].
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

/// Allows one pairing at a time, so a device in pairing mode can't be
/// flooded with confirmation dialogs showing different codes.
#[derive(Debug, Default)]
pub struct PairingSlot(AtomicBool);

/// Holds a [`PairingSlot`] until dropped.
#[derive(Debug)]
pub struct PairingGuard<'a>(&'a PairingSlot);

impl PairingSlot {
    pub const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// `None` if a pairing is already in progress.
    pub fn try_take(&self) -> Option<PairingGuard<'_>> {
        self.0
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| PairingGuard(self))
    }
}

impl Drop for PairingGuard<'_> {
    fn drop(&mut self) {
        self.0.0.store(false, Ordering::Release);
    }
}

/// The process-wide slot [`Options::new`] uses.
pub static PAIRING_SLOT: PairingSlot = PairingSlot::new();

pub struct Options<'a> {
    pub identity: &'a Identity,
    /// This device's name, sent to the peer.
    pub name: &'a str,
    pub trust: &'a Mutex<TrustStore>,
    /// Whether this side accepts pairing with unknown devices right now (on
    /// the primary: only while the user has pairing mode open).
    pub can_pair: bool,
    /// Asks the user whether the code matches the other screen. Called on
    /// the handshake's thread; may block until the user answers (answers
    /// after [`PairingRequest::deadline`] count as "no").
    pub confirm: &'a (dyn Fn(&PairingRequest) -> bool + Sync),
    /// Limit for everything automatic: Noise handshake, trust exchange and
    /// the pairing-code exchange, however slowly the peer sends.
    pub handshake_timeout: Duration,
    /// Limit for both users to confirm the code.
    pub pairing_timeout: Duration,
    /// Only one pairing at a time may hold this.
    pub pairing_slot: &'a PairingSlot,
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
            pairing_slot: &PAIRING_SLOT,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Noise(snow::Error),
    Protocol(String),
    /// The handshake (before any user interaction) took too long.
    HandshakeTimeout,
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
    /// Another pairing is already waiting for the user here.
    PairingBusy,
    /// The peer's revealed pairing nonce doesn't match its commitment: it
    /// tried to choose the code. Treat as an attack.
    CommitmentMismatch,
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
            Self::HandshakeTimeout => write!(f, "handshake timed out"),
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
            Self::PairingBusy => write!(f, "another pairing is already in progress"),
            Self::CommitmentMismatch => write!(
                f,
                "the other device cheated in the pairing-code exchange; someone may be in the middle"
            ),
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

fn is_timeout(e: &Error) -> bool {
    matches!(e, Error::Io(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock))
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
    let deadline = Instant::now() + opts.handshake_timeout;
    let (secure, peer) = establish_inner(stream, opts, initiator, deadline)?;
    // Best effort: macOS returns EINVAL for setsockopt once the peer has
    // closed, and the handshake's own result matters more.
    let _ = secure.set_read_timeout(previous_timeout);
    Ok((secure, peer))
}

fn establish_inner(
    mut stream: TcpStream,
    opts: &Options,
    initiator: bool,
    deadline: Instant,
) -> Result<(SecureStream, Peer), Error> {
    let timeout = |e: Error| {
        if is_timeout(&e) {
            Error::HandshakeTimeout
        } else {
            e
        }
    };
    let name = opts.name.as_bytes();
    let name = &name[..floor_char_boundary(opts.name, MAX_NAME_LEN.min(name.len()))];
    let builder = snow::Builder::new(NOISE_PARAMS.parse()?)
        .prologue(PROLOGUE)?
        .local_private_key(opts.identity.private_key())?;

    // XX: -> e; <- e, ee, s, es [name]; -> s, se [name]
    let (hs, peer_name) = if initiator {
        let mut hs = builder.build_initiator()?;
        send_handshake(&mut stream, &mut hs, &[])?;
        let peer_name = recv_handshake(&stream, &mut hs, deadline).map_err(timeout)?;
        send_handshake(&mut stream, &mut hs, name)?;
        (hs, peer_name)
    } else {
        let mut hs = builder.build_responder()?;
        recv_handshake(&stream, &mut hs, deadline).map_err(timeout)?;
        send_handshake(&mut stream, &mut hs, name)?;
        let peer_name = recv_handshake(&stream, &mut hs, deadline).map_err(timeout)?;
        (hs, peer_name)
    };
    // Sanitized once, here: the store, the dialog and `Peer` all see the same.
    let peer_name = sanitize_name(
        &String::from_utf8(peer_name)
            .map_err(|_| Error::Protocol("peer name is not UTF-8".into()))?,
    );
    let key: PublicKey = hs
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| Error::Protocol("no remote static key".into()))?;
    let hash: [u8; 32] = hs
        .get_handshake_hash()
        .try_into()
        .map_err(|_| Error::Protocol("unexpected handshake hash length".into()))?;
    let state = hs.into_stateless_transport_mode()?;
    // The reader keeps the original handle: on Windows, socket options such
    // as the read timeout aren't shared with `try_clone`d handles.
    let writer = stream.try_clone()?;
    let (reader, writer) = halves(state, stream, writer);
    let mut secure = SecureStream { reader, writer };

    let newly_paired = exchange_trust(
        &mut secure,
        opts,
        initiator,
        &peer_name,
        &key,
        &hash,
        deadline,
    )?;
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
    opts: &Options,
    initiator: bool,
    peer_name: &str,
    key: &PublicKey,
    hash: &[u8; 32],
    deadline: Instant,
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
    let &[theirs, peer_can_pair] = read_record(secure, deadline)?.as_slice() else {
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

    // Pairing. One at a time here; the other side notices us closing.
    let _slot = opts.pairing_slot.try_take().ok_or(Error::PairingBusy)?;
    let code = exchange_code(secure, initiator, hash, deadline)?;

    // Both users compare the code; each side sends one record.
    let pairing_deadline = Instant::now() + opts.pairing_timeout;
    let request = PairingRequest {
        peer_name: peer_name.into(),
        peer_fingerprint: fingerprint(key),
        code,
        deadline: pairing_deadline,
    };
    let accepted = (opts.confirm)(&request) && Instant::now() < pairing_deadline;
    secure.writer.write_record(&[accepted.into()])?;
    if !accepted {
        return Err(Error::Declined { by_peer: false });
    }
    let answer = match read_record(secure, pairing_deadline) {
        Ok(answer) => answer,
        Err(e) if is_timeout(&e) => return Err(Error::PairingTimeout),
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

/// Commit-then-reveal, so neither side (nor a man in the middle running two
/// handshakes) can choose its input after seeing the other's:
/// initiator → `C = H("onemouse-sas-commit" ‖ h ‖ Ni)`, responder → `Nr`,
/// initiator → `Ni` (checked against `C`). The code is
/// `H("onemouse-sas" ‖ h ‖ Ni ‖ Nr)`.
fn exchange_code(
    secure: &mut SecureStream,
    initiator: bool,
    hash: &[u8; 32],
    deadline: Instant,
) -> Result<String, Error> {
    let read32 = |secure: &mut SecureStream| -> Result<[u8; 32], Error> {
        match read_record(secure, deadline) {
            Ok(record) => record
                .try_into()
                .map_err(|_| Error::Protocol("malformed pairing-code record".into())),
            Err(e) if is_timeout(&e) => Err(Error::HandshakeTimeout),
            Err(e) => Err(e),
        }
    };
    if initiator {
        let ni = random_nonce()?;
        secure.writer.write_record(&commitment(hash, &ni))?;
        let nr = read32(secure)?;
        secure.writer.write_record(&revealed(ni))?;
        Ok(pairing_code(hash, &ni, &nr))
    } else {
        let committed = read32(secure)?;
        let nr = random_nonce()?;
        secure.writer.write_record(&nr)?;
        let ni = read32(secure)?;
        if !bool::from(commitment(hash, &ni).ct_eq(&committed)) {
            return Err(Error::CommitmentMismatch);
        }
        Ok(pairing_code(hash, &ni, &nr))
    }
}

fn random_nonce() -> Result<[u8; 32], Error> {
    let mut nonce = [0; 32];
    getrandom::fill(&mut nonce)
        .map_err(|e| Error::Io(io::Error::other(format!("no randomness: {e}"))))?;
    Ok(nonce)
}

fn commitment(hash: &[u8; 32], ni: &[u8; 32]) -> [u8; 32] {
    Blake2s256::new()
        .chain_update(b"onemouse-sas-commit")
        .chain_update(hash)
        .chain_update(ni)
        .finalize()
        .into()
}

/// Six decimal digits, e.g. `"042 917"`, from the final handshake hash `h`
/// (which binds both static keys) and both commit-reveal nonces.
pub fn pairing_code(hash: &[u8; 32], ni: &[u8; 32], nr: &[u8; 32]) -> String {
    let digest: [u8; 32] = Blake2s256::new()
        .chain_update(b"onemouse-sas")
        .chain_update(hash)
        .chain_update(ni)
        .chain_update(nr)
        .finalize()
        .into();
    let n = u64::from_be_bytes(digest[..8].try_into().expect("32-byte digest")) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

thread_local! {
    /// Test hook: make this thread's initiator reveal a different nonce
    /// than it committed to.
    static CHEAT_REVEAL: Cell<bool> = const { Cell::new(false) };
}

fn revealed(mut ni: [u8; 32]) -> [u8; 32] {
    if cfg!(test) && CHEAT_REVEAL.get() {
        ni[0] ^= 1;
    }
    ni
}

fn read_record(secure: &mut SecureStream, deadline: Instant) -> Result<Vec<u8>, Error> {
    match secure.reader.read_record_by(deadline)? {
        Some(record) => Ok(record.to_vec()),
        None => Err(Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed the connection during the handshake",
        ))),
    }
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

fn recv_handshake(
    stream: &TcpStream,
    hs: &mut HandshakeState,
    deadline: Instant,
) -> Result<Vec<u8>, Error> {
    let mut len = [0; 2];
    read_exact_by(stream, &mut len, deadline)?;
    let mut msg = vec![0; u16::from_be_bytes(len) as usize];
    read_exact_by(stream, &mut msg, deadline)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn code_depends_on_the_hash_and_both_nonces() {
        let (h, ni, nr) = ([1; 32], [2; 32], [3; 32]);
        let code = pairing_code(&h, &ni, &nr);
        assert_eq!(code.len(), 7);
        assert_eq!(code, pairing_code(&h, &ni, &nr));
        assert_ne!(code, pairing_code(&[9; 32], &ni, &nr));
        assert_ne!(code, pairing_code(&h, &[9; 32], &nr));
        assert_ne!(code, pairing_code(&h, &ni, &[9; 32]));
        // Swapping the nonces' roles changes it too.
        assert_ne!(code, pairing_code(&h, &nr, &ni));
    }

    #[test]
    fn a_reveal_that_does_not_match_the_commitment_aborts() {
        let identities = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (pc_trust, mac_trust) = (
            Mutex::new(TrustStore::in_memory()),
            Mutex::new(TrustStore::in_memory()),
        );
        let (pc_slot, mac_slot) = (PairingSlot::new(), PairingSlot::new());
        // Only the Mac verifies the reveal, so only its user must never be
        // asked. (The PC's user may see a code; nothing gets pinned anyway.)
        let mac_asked = AtomicBool::new(false);
        let mac_confirm = |_: &PairingRequest| {
            mac_asked.store(true, Ordering::SeqCst);
            true
        };
        let pc_confirm = |_: &PairingRequest| true;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, server) = thread::scope(|s| {
            let server = s.spawn(|| {
                let opts = Options {
                    can_pair: true,
                    confirm: &mac_confirm,
                    pairing_slot: &mac_slot,
                    ..Options::new(&identities.1, "mac", &mac_trust)
                };
                accept(listener.accept().unwrap().0, &opts)
            });
            CHEAT_REVEAL.set(true);
            let opts = Options {
                can_pair: true,
                confirm: &pc_confirm,
                pairing_slot: &pc_slot,
                ..Options::new(&identities.0, "pc", &pc_trust)
            };
            let client = connect(TcpStream::connect(addr).unwrap(), &opts);
            CHEAT_REVEAL.set(false);
            (client, server.join().unwrap())
        });
        let server = server.unwrap_err();
        assert!(matches!(server, Error::CommitmentMismatch), "{server:?}");
        assert!(client.is_err());
        assert!(
            !mac_asked.load(Ordering::SeqCst),
            "no code shown to the mac's user"
        );
        assert!(mac_trust.lock().unwrap().peers().is_empty());
        assert!(pc_trust.lock().unwrap().peers().is_empty());
    }

    #[test]
    fn pairing_slot_is_exclusive() {
        let slot = PairingSlot::new();
        let guard = slot.try_take().unwrap();
        assert!(slot.try_take().is_none());
        drop(guard);
        assert!(slot.try_take().is_some());
    }
}
