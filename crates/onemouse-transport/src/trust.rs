//! Pinned peers: which public key belongs to which device name.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hex;
use crate::identity::{PublicKey, write_private};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedPeer {
    pub name: String,
    pub key: PublicKey,
    /// Unix seconds.
    pub paired_at: u64,
}

/// What we know about a peer presenting `name` and `key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// This key is pinned.
    Pinned,
    /// Never seen: pairing needed.
    Unknown,
    /// `name` is pinned to a different key. Never replaced automatically.
    KeyChanged,
}

/// Stored as `name<TAB>key-hex<TAB>paired-at` lines, user-only permissions.
#[derive(Debug, Default)]
pub struct TrustStore {
    path: Option<PathBuf>,
    peers: Vec<PinnedPeer>,
}

impl TrustStore {
    /// A store that is never saved (tests, or a "don't remember" mode).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Loads `path`, or starts empty if it doesn't exist yet.
    pub fn load(path: &Path) -> io::Result<Self> {
        let peers = match fs::read_to_string(path) {
            Ok(text) => text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|line| {
                    parse_line(line).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("bad line in {}: {line:?}", path.display()),
                        )
                    })
                })
                .collect::<io::Result<_>>()?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path: Some(path.to_owned()),
            peers,
        })
    }

    pub fn peers(&self) -> &[PinnedPeer] {
        &self.peers
    }

    pub fn check(&self, name: &str, key: &PublicKey) -> Trust {
        if self.peers.iter().any(|p| &p.key == key) {
            Trust::Pinned
        } else if self.peers.iter().any(|p| p.name == name) {
            Trust::KeyChanged
        } else {
            Trust::Unknown
        }
    }

    /// Pins `key` under `name` (renaming it if the key is already pinned) and
    /// saves. Refuses to replace a different key already pinned under that
    /// name: [`TrustStore::forget`] it first.
    pub fn pin(&mut self, name: &str, key: &PublicKey) -> io::Result<()> {
        let name = sanitize(name);
        let paired_at = self
            .peers
            .iter()
            .find(|p| &p.key == key)
            .map(|p| p.paired_at);
        if self.peers.iter().any(|p| p.name == name && &p.key != key) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{name} is already pinned to a different key"),
            ));
        }
        self.peers.retain(|p| &p.key != key);
        // A rename keeps the original pairing date.
        let paired_at = paired_at.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        });
        self.peers.push(PinnedPeer {
            name,
            key: *key,
            paired_at,
        });
        self.save()
    }

    /// Removes every peer pinned under `name`. Returns whether any was.
    pub fn forget(&mut self, name: &str) -> io::Result<bool> {
        let before = self.peers.len();
        self.peers.retain(|p| p.name != name);
        if self.peers.len() == before {
            return Ok(false);
        }
        self.save().map(|()| true)
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let text: String = self
            .peers
            .iter()
            .map(|p| format!("{}\t{}\t{}\n", p.name, hex::encode(&p.key), p.paired_at))
            .collect();
        write_private(path, text.as_bytes())
    }
}

fn parse_line(line: &str) -> Option<PinnedPeer> {
    let mut fields = line.split('\t');
    let peer = PinnedPeer {
        name: fields.next()?.to_owned(),
        key: hex::decode(fields.next()?)?.try_into().ok()?,
        paired_at: fields.next()?.parse().ok()?,
    };
    fields.next().is_none().then_some(peer)
}

/// Names are free text from the peer; keep the file format intact.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const K1: PublicKey = [1; 32];
    const K2: PublicKey = [2; 32];

    #[test]
    fn check_pin_and_key_change() {
        let mut store = TrustStore::in_memory();
        assert_eq!(store.check("mac", &K1), Trust::Unknown);
        store.pin("mac", &K1).unwrap();
        assert_eq!(store.check("mac", &K1), Trust::Pinned);
        // A renamed device keeps its key.
        assert_eq!(store.check("renamed mac", &K1), Trust::Pinned);
        assert_eq!(store.check("mac", &K2), Trust::KeyChanged);
        let err = store.pin("mac", &K2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(store.forget("mac").unwrap());
        assert!(!store.forget("mac").unwrap());
        assert_eq!(store.check("mac", &K2), Trust::Unknown);
    }

    #[test]
    fn persists_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peers");
        let mut store = TrustStore::load(&path).unwrap();
        store.pin("jow's\tMacBook\nAir", &K1).unwrap();
        store.pin("desk", &K2).unwrap();
        let loaded = TrustStore::load(&path).unwrap();
        assert_eq!(loaded.peers(), store.peers());
        assert_eq!(loaded.peers()[0].name, "jow's MacBook Air");
        assert!(loaded.peers()[0].paired_at > 0);
    }

    #[test]
    fn rejects_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peers");
        fs::write(&path, "mac\tnothex\t0\n").unwrap();
        assert_eq!(
            TrustStore::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn store_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peers");
        TrustStore::load(&path).unwrap().pin("mac", &K1).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
