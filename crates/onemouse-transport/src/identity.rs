//! This device's long-lived X25519 static keypair, and where onemouse keeps
//! its files.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// run. The file is readable by the current user only. Two processes
    /// starting at once end up with the same identity: creation is exclusive.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match Self::load(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            other => return other,
        }
        let identity = Self::generate()?;
        let text = format!(
            "{}\n{}\n",
            hex::encode(&identity.private),
            hex::encode(&identity.public)
        );
        match write_private_new(path, text.as_bytes()) {
            Ok(()) => Ok(identity),
            // Someone else created it first: use theirs.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Self::load(path),
            Err(e) => Err(e),
        }
    }

    fn load(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        Self::parse(&text).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a valid onemouse identity", path.display()),
            )
        })
    }

    /// Both keys, and the public one must belong to the private one.
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let private: [u8; KEY_LEN] = hex::decode(lines.next()?)?.try_into().ok()?;
        let public: PublicKey = hex::decode(lines.next()?)?.try_into().ok()?;
        (public_from_private(&private)? == public).then_some(Self { private, public })
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

fn public_from_private(private: &[u8; KEY_LEN]) -> Option<PublicKey> {
    use snow::params::DHChoice;
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    let mut dh = DefaultResolver.resolve_dh(&DHChoice::Curve25519)?;
    dh.set(private);
    dh.pubkey().try_into().ok()
}

/// Short, stable identifier of a public key, for display and mDNS TXT records.
/// Not a security check by itself: pairing codes are.
pub fn fingerprint(key: &PublicKey) -> String {
    hex::encode(&key[..8])
}

/// Writes `contents` to `path` atomically (temp file + rename), readable by
/// the current user only: mode 0600 in a 0700 directory on Unix, a protected
/// DACL granting only the current user on Windows. Replaces `path`.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = write_temp(path, contents)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Like [`write_private`], but fails with `AlreadyExists` instead of
/// replacing an existing `path`.
fn write_private_new(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = write_temp(path, contents)?;
    let linked = fs::hard_link(&tmp, path);
    let _ = fs::remove_file(&tmp);
    linked
}

/// Writes a fully private temp file next to `path`. The name is unique per
/// process and call, so concurrent writers never share one.
fn write_temp(path: &Path, contents: &[u8]) -> io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(name);

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    let written = (|| {
        // Lock it down before any secret goes in.
        #[cfg(windows)]
        windows_acl::restrict_to_current_user(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()
    })();
    drop(file);
    match written {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    // `mode` only applies to directories created just now.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) mod windows_acl {
    //! Owner-only DACLs, not inherited from `%APPDATA%` (which a roaming or
    //! redirected profile may have widened). Granted to the current user's
    //! SID rather than "owner rights": under an elevated admin the owner can
    //! be the Administrators group.

    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// The current user's SID as a string, e.g. `S-1-5-21-…`.
    fn current_user_sid() -> io::Result<String> {
        // SAFETY: standard token query. The buffer outlives the SID pointer,
        // and the token handle and the SID string are released.
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buf = vec![0u8; 512];
            let mut needed = 0;
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut needed,
            );
            let err = io::Error::last_os_error();
            CloseHandle(token);
            if ok == 0 {
                return Err(err);
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut text: *mut u16 = ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            Ok(sid)
        }
    }

    /// Replaces `path`'s DACL with a protected one granting full control to
    /// the current user only.
    pub fn restrict_to_current_user(path: &Path) -> io::Result<()> {
        let sddl: Vec<u16> = format!("D:P(A;;FA;;;{})\0", current_user_sid()?)
            .encode_utf16()
            .collect();
        // SAFETY: the descriptor is freed after use; the DACL points into it.
        unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let (mut present, mut defaulted) = (0, 0);
            let mut dacl: *mut ACL = ptr::null_mut();
            let result =
                if GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    match SetNamedSecurityInfoW(
                        wide(path).as_ptr(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        dacl,
                        ptr::null(),
                    ) {
                        ERROR_SUCCESS => Ok(()),
                        err => Err(io::Error::from_raw_os_error(err as i32)),
                    }
                };
            LocalFree(sd);
            result
        }
    }

    /// (number of ACEs, protected from inheritance) of `path`'s DACL.
    #[cfg(test)]
    pub fn describe(path: &Path) -> (u16, bool) {
        use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
        use windows_sys::Win32::Security::{GetSecurityDescriptorControl, SE_DACL_PROTECTED};
        // SAFETY: test-only read of a descriptor we free afterwards.
        unsafe {
            let (mut dacl, mut sd): (*mut ACL, PSECURITY_DESCRIPTOR) =
                (ptr::null_mut(), ptr::null_mut());
            let err = GetNamedSecurityInfoW(
                wide(path).as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut sd,
            );
            assert_eq!(err, ERROR_SUCCESS);
            let (mut control, mut revision) = (0, 0);
            GetSecurityDescriptorControl(sd, &mut control, &mut revision);
            let result = ((*dacl).AceCount, control & SE_DACL_PROTECTED != 0);
            LocalFree(sd);
            result
        }
    }
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
    fn rejects_a_public_key_that_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let text = format!("{}\n{}\n", hex::encode(&a.private), hex::encode(&b.public));
        fs::write(&path, text).unwrap();
        let err = Identity::load_or_create(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn concurrent_first_runs_agree_on_one_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        let keys: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| *Identity::load_or_create(&path).unwrap().public_key()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(keys.windows(2).all(|w| w[0] == w[1]));
        // No temp files left behind.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn existing_config_dir_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("onemouse");
        fs::create_dir(&config).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o755)).unwrap();
        Identity::load_or_create(&config.join("identity")).unwrap();
        let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[cfg(windows)]
    #[test]
    fn key_file_is_private_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("onemouse").join("identity");
        Identity::load_or_create(&path).unwrap();
        assert_eq!(
            windows_acl::describe(&path),
            (1, true),
            "one ACE, not inherited"
        );
    }

    #[test]
    fn debug_does_not_leak_the_private_key() {
        let id = Identity::generate().unwrap();
        let debug = format!("{id:?}");
        assert!(!debug.contains(&hex::encode(id.private_key())));
        assert!(debug.contains(&id.fingerprint()));
    }
}
