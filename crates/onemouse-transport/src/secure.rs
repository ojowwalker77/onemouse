//! The encrypted byte stream: `u16`-BE length + ChaCha20-Poly1305 ciphertext
//! records, with implicit per-direction nonces. Reader and writer halves are
//! independent so they can live on different threads.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;

use snow::StatelessTransportState;

/// Largest Noise message (record body), tag included.
pub const MAX_RECORD: usize = 65535;
const TAG_LEN: usize = 16;
/// Largest plaintext carried by one record.
pub const MAX_PLAINTEXT: usize = MAX_RECORD - TAG_LEN;

/// Decrypting half. `Read` yields the plaintext byte stream.
pub struct SecureReader<S> {
    inner: S,
    state: Arc<StatelessTransportState>,
    nonce: u64,
    record: Vec<u8>,
    plain: Vec<u8>,
    plain_len: usize,
    pos: usize,
}

/// Encrypting half. Each `write` call becomes one record (split at
/// [`MAX_PLAINTEXT`]), so a `write_all` of a small frame is one TCP send.
pub struct SecureWriter<S> {
    inner: S,
    state: Arc<StatelessTransportState>,
    nonce: u64,
    out: Vec<u8>,
}

pub(crate) fn halves<R, W>(
    state: StatelessTransportState,
    reader: R,
    writer: W,
) -> (SecureReader<R>, SecureWriter<W>) {
    let state = Arc::new(state);
    (
        SecureReader {
            inner: reader,
            state: Arc::clone(&state),
            nonce: 0,
            record: vec![0; MAX_RECORD],
            plain: vec![0; MAX_RECORD],
            plain_len: 0,
            pos: 0,
        },
        SecureWriter {
            inner: writer,
            state,
            nonce: 0,
            out: vec![0; 2 + MAX_RECORD],
        },
    )
}

fn bad_data(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

impl<S: Read> SecureReader<S> {
    /// Reads and decrypts one whole record. `Ok(None)` on a clean EOF.
    pub(crate) fn read_record(&mut self) -> io::Result<Option<&[u8]>> {
        let mut len = [0; 2];
        match self.inner.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u16::from_be_bytes(len) as usize;
        if len < TAG_LEN {
            return Err(bad_data("record shorter than its authentication tag"));
        }
        self.inner.read_exact(&mut self.record[..len])?;
        let n = self
            .state
            .read_message(self.nonce, &self.record[..len], &mut self.plain)
            .map_err(|_| bad_data("record failed authentication"))?;
        self.nonce += 1;
        Ok(Some(&self.plain[..n]))
    }

    /// Bytes decrypted but not yet consumed.
    fn buffered(&self) -> usize {
        self.plain_len - self.pos
    }

    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

impl<S: Read> Read for SecureReader<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.buffered() == 0 {
            match self.read_record()? {
                Some(plain) => {
                    let n = plain.len();
                    self.plain_len = n;
                    self.pos = 0;
                }
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.buffered());
        buf[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl<S: Write> SecureWriter<S> {
    /// Encrypts `plain` (at most [`MAX_PLAINTEXT`] bytes) as one record.
    pub(crate) fn write_record(&mut self, plain: &[u8]) -> io::Result<()> {
        debug_assert!(plain.len() <= MAX_PLAINTEXT);
        let n = self
            .state
            .write_message(self.nonce, plain, &mut self.out[2..])
            .map_err(|e| io::Error::other(format!("encryption failed: {e}")))?;
        self.nonce += 1;
        self.out[..2].copy_from_slice(&(n as u16).to_be_bytes());
        self.inner.write_all(&self.out[..2 + n])
    }

    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

impl<S: Write> Write for SecureWriter<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(MAX_PLAINTEXT);
        self.write_record(&buf[..n])?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// An authenticated, encrypted connection to a peer.
pub struct SecureStream {
    pub(crate) reader: SecureReader<TcpStream>,
    pub(crate) writer: SecureWriter<TcpStream>,
}

impl std::fmt::Debug for SecureStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureStream")
            .field("peer", &self.tcp().peer_addr().ok())
            .finish_non_exhaustive()
    }
}

impl SecureStream {
    /// Splits into halves for a reader and a writer thread.
    pub fn split(self) -> (SecureReader<TcpStream>, SecureWriter<TcpStream>) {
        (self.reader, self.writer)
    }

    /// The underlying socket, e.g. for timeouts or `peer_addr`.
    pub fn tcp(&self) -> &TcpStream {
        self.reader.get_ref()
    }

    pub fn shutdown(&self) -> io::Result<()> {
        self.tcp().shutdown(Shutdown::Both)
    }
}

impl Read for SecureStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Write for SecureStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NOISE_PARAMS;

    /// A completed XX handshake between two in-process states.
    fn transport_pair() -> (StatelessTransportState, StatelessTransportState) {
        let builder = || snow::Builder::new(NOISE_PARAMS.parse().unwrap());
        let (ik, rk) = (
            builder().generate_keypair().unwrap(),
            builder().generate_keypair().unwrap(),
        );
        let mut i = builder()
            .local_private_key(&ik.private)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut r = builder()
            .local_private_key(&rk.private)
            .unwrap()
            .build_responder()
            .unwrap();
        let (mut msg, mut payload) = (vec![0; 65535], vec![0; 65535]);
        let n = i.write_message(&[], &mut msg).unwrap();
        r.read_message(&msg[..n], &mut payload).unwrap();
        let n = r.write_message(&[], &mut msg).unwrap();
        i.read_message(&msg[..n], &mut payload).unwrap();
        let n = i.write_message(&[], &mut msg).unwrap();
        r.read_message(&msg[..n], &mut payload).unwrap();
        (
            i.into_stateless_transport_mode().unwrap(),
            r.into_stateless_transport_mode().unwrap(),
        )
    }

    /// Encrypts `chunks` (one `write_all` each) from initiator to responder.
    fn encrypt(chunks: &[&[u8]]) -> (Vec<u8>, StatelessTransportState) {
        let (i, r) = transport_pair();
        let (_, mut writer) = halves(i, io::empty(), Vec::new());
        for chunk in chunks {
            writer.write_all(chunk).unwrap();
        }
        (writer.inner, r)
    }

    fn reader(wire: Vec<u8>, state: StatelessTransportState) -> SecureReader<io::Cursor<Vec<u8>>> {
        halves(state, io::Cursor::new(wire), io::sink()).0
    }

    #[test]
    fn round_trips_large_writes_across_records() {
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let (wire, r) = encrypt(&[b"hello", &big, b""]);
        // 1 record + ceil(300000 / 65519) records.
        let records = 1 + big.len().div_ceil(MAX_PLAINTEXT);
        assert_eq!(wire.len(), 5 + big.len() + records * (2 + TAG_LEN));
        let mut plain = Vec::new();
        reader(wire, r).read_to_end(&mut plain).unwrap();
        assert_eq!(&plain[..5], b"hello");
        assert_eq!(&plain[5..], &big[..]);
    }

    #[test]
    fn flipped_bit_fails_authentication() {
        let (mut wire, r) = encrypt(&[b"press A"]);
        let last = wire.len() - 1;
        wire[last] ^= 0x01;
        let err = reader(wire, r).read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn replayed_or_reordered_records_fail() {
        let (wire, r) = encrypt(&[b"one", b"two"]);
        let first = wire[..2 + 3 + TAG_LEN].to_vec();
        let replayed = [first.clone(), first].concat();
        let mut reader = reader(replayed, r);
        let mut buf = [0; 3];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"one");
        let err = reader.read_exact(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_record_is_an_error_not_eof() {
        let (mut wire, r) = encrypt(&[b"press A"]);
        wire.truncate(wire.len() - 3);
        let err = reader(wire, r).read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
