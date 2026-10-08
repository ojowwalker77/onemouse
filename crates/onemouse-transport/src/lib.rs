//! Encrypted, mutually authenticated transport for onemouse (M2).
//!
//! - [`handshake`]: Noise `XX_25519_ChaChaPoly_BLAKE2s` over TCP (the
//!   secondary initiates), then a trust exchange: pinned keys connect
//!   silently, unknown keys go through pairing (both users confirm the same
//!   6-digit code), a name pinned to a different key is a hard failure.
//! - [`SecureStream`]: `Read + Write` over encrypted records, so
//!   `onemouse_protocol::{read_message, write_message}` work unchanged.
//! - [`Identity`] and [`TrustStore`]: this device's static key and pinned
//!   peers, kept in [`config_dir`] with user-only permissions.
//! - [`discovery`]: mDNS advertise/browse.
//!
//! No platform-specific code: builds and tests everywhere.
//!
//! # Wire format
//!
//! Everything below happens on the TCP connection before the first
//! `onemouse_protocol::Message`. Steps 1–3 (and the code exchange in 4) must
//! finish within one overall deadline (10 s by default), however slowly the
//! peer sends.
//!
//! 1. **Noise handshake** `Noise_XX_25519_ChaChaPoly_BLAKE2s`, prologue
//!    `"onemouse transport v1"`. The secondary is the initiator. Each
//!    handshake message is framed as a `u16` big-endian length + the message.
//!    The payload of message 2 (responder) and message 3 (initiator) is the
//!    sender's device name, UTF-8, at most 255 bytes; the receiver sanitizes
//!    it (control characters → spaces, bidi overrides removed) before any use.
//! 2. **Records**: `u16` big-endian length + ciphertext (16-byte tag
//!    included, ≤ 65535 bytes). Nonces are implicit counters starting at 0,
//!    one per direction; a dropped, replayed or reordered record fails
//!    authentication, and any error leaves that direction unusable. The
//!    plaintext of consecutive records is one byte stream, so frames of any
//!    size (up to `MAX_FRAME_LEN`) span records.
//! 3. **Trust exchange**: each side sends one record `[trust, can_pair]`,
//!    `trust` = 0 pinned / 1 unknown / 2 your key differs from the one
//!    pinned under your name. Resolution, identical on both sides:
//!    any 2 → close (never re-pair automatically); both 0 → done;
//!    otherwise pairing, which needs `can_pair = 1` on both sides.
//!    A side only ever trusts its *own* store: the peer's byte can make it
//!    stricter, never looser.
//! 4. **Pairing code**, commit-then-reveal so that nobody (including a man
//!    in the middle running two handshakes) can steer it: the initiator sends
//!    `C = BLAKE2s("onemouse-sas-commit" ‖ h ‖ Ni)`, the responder sends
//!    `Nr`, the initiator sends `Ni` and the responder checks it against `C`
//!    (mismatch → close). `h` is the final handshake hash (it binds both
//!    static keys); `Ni`, `Nr` are 32 random bytes. Both sides show
//!    `u64be(BLAKE2s("onemouse-sas" ‖ h ‖ Ni ‖ Nr)[0..8]) % 1_000_000` and
//!    ask their user; one pairing at a time per process.
//! 5. **Confirmation**: each side sends one record `[1]` (match) or `[0]`.
//!    A side pins the peer only when both answered 1 within 60 s.
//! 6. Then the `onemouse_protocol` frames, inside records.

pub mod discovery;
pub mod handshake;
mod hex;
pub mod identity;
pub mod secure;
pub mod trust;

pub use handshake::{
    Error, Options, PAIRING_SLOT, PairingGuard, PairingRequest, PairingSlot, Peer, accept, connect,
    pairing_code,
};
pub use identity::{Identity, PublicKey, config_dir, fingerprint};
pub use secure::{SecureReader, SecureStream, SecureWriter};
pub use trust::{PinnedPeer, Trust, TrustStore};

pub(crate) const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
/// Mixed into the handshake: both sides must agree they speak onemouse.
pub(crate) const PROLOGUE: &[u8] = b"onemouse transport v1";

/// File names inside [`config_dir`].
pub const IDENTITY_FILE: &str = "identity";
pub const PEERS_FILE: &str = "peers";
