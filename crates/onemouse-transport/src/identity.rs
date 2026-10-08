//! This device's long-lived X25519 static keypair, and where onemouse keeps
//! its files.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::{NOISE_PARAMS, hex};

/// Length of an X25519 public or private key.
pub const KEY_LEN: usize = 32;

pub type PublicKey = [u8; KEY_LEN];

/// Per-user config directory: `%APPDATA%\onemouse` on Windows,
/// `~/Library/Application Support/onemouse` on macOS,
/// `$XDG_CONFIG_HOME/onemouse` (or `~/.config/onemouse`) elsewhere.
pub fn config_dir() -> io::Result<PathBuf> {
    let missing = |var| io::Error::new(io::ErrorKind::NotFound, format!("{var} is not set"));
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA").ok_or_else(|| missing("APPDATA"))?)
    } else {
        let home = || {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| missing("HOME"))
        };
        if cfg!(target_os = "macos") {
            home()?.join("Library").join("Application Support")
        } else {
            match std::env::var_os("XDG_CONFIG_HOME") {
                Some(dir) if !dir.is_empty() => PathBuf::from(dir),
                _ => home()?.join(".config"),
            }
        }
    };
    Ok(base.join("onemouse"))
}

pub struct Identity {
    private: [u8; KEY_LEN],
    public: PublicKey,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl Identity {
    pub fn generate() -> io::Result<Self> {
        let keypair = snow::Builder::new(NOISE_PARAMS.parse().expect("valid Noise params"))
            .generate_keypair()
            .map_err(|e| io::Error::other(format!("can't generate a keypair: {e}")))?;
        Ok(Self {
            private: keypair.private.try_into().expect("X25519 private key"),
            public: keypair.public.try_into().expect("X25519 public key"),
        })
    }

    /// Loads the identity at `path`, creating it (and its directory) on first
    /// run. The file is readable by the current user only.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a valid onemouse identity", path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate()?;
                let text = format!(
                    "{}\n{}\n",
                    hex::encode(&identity.private),
                    hex::encode(&identity.public)
                );
                write_private(path, text.as_bytes())?;
                Ok(identity)
            }
            Err(e) => Err(e),
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let private = hex::decode(lines.next()?)?.try_into().ok()?;
        let public = hex::decode(lines.next()?)?.try_into().ok()?;
        Some(Self { private, public })
    }

    pub fn public_key(&self) -> &PublicKey {
        &self.public
    }

    pub(crate) fn private_key(&self) -> &[u8; KEY_LEN] {
        &self.private
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }
}

/// Short, stable identifier of a public key, for display and mDNS TXT records.
/// Not a security check by itself: pairing codes are.
pub fn fingerprint(key: &PublicKey) -> String {
    hex::encode(&key[..8])
}

/// Writes `contents` to `path` atomically (temp file + rename), readable by
/// the current user only: mode 0600 in a 0700 directory on Unix. On Windows
/// files under `%APPDATA%` inherit the per-user ACL of the profile.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    #[cfg(unix)]
    {
        // `mode` only applies on creation; a stale temp file keeps its old one.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_once_then_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("identity");
        let first = Identity::load_or_create(&path).unwrap();
        let second = Identity::load_or_create(&path).unwrap();
        assert_eq!(first.public_key(), second.public_key());
        assert_eq!(first.private_key(), second.private_key());
        assert_eq!(first.fingerprint().len(), 16);
        assert_ne!(
            first.public_key(),
            Identity::generate().unwrap().public_key()
        );
    }

    #[test]
    fn rejects_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        fs::write(&path, "not hex\n").unwrap();
        let err = Identity::load_or_create(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("onemouse").join("identity");
        Identity::load_or_create(&path).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn debug_does_not_leak_the_private_key() {
        let id = Identity::generate().unwrap();
        let debug = format!("{id:?}");
        assert!(!debug.contains(&hex::encode(id.private_key())));
        assert!(debug.contains(&id.fingerprint()));
    }
}
