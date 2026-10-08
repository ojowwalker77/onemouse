//! Start at login via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`,
//! plus a guard against running twice.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::CreateMutexW;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "onemouse";

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// Quotes one argument for a Windows command line.
pub fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Runs this executable with `args` at every login.
pub fn install(args: &[String]) -> io::Result<String> {
    let exe = std::env::current_exe()?;
    let command = std::iter::once(quote(&exe.to_string_lossy()))
        .chain(args.iter().map(|a| quote(a)))
        .collect::<Vec<_>>()
        .join(" ");
    let data = wide(&command);
    // SAFETY: valid NUL-terminated strings; the data length is in bytes.
    let err = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            wide(VALUE).as_ptr(),
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    match err {
        0 => Ok(command),
        err => Err(io::Error::from_raw_os_error(err as i32)),
    }
}

/// Removes the login entry. `Ok(false)` if there wasn't one.
pub fn uninstall() -> io::Result<bool> {
    // SAFETY: valid NUL-terminated strings.
    let err = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            wide(VALUE).as_ptr(),
        )
    };
    match err {
        0 => Ok(true),
        ERROR_FILE_NOT_FOUND => Ok(false),
        err => Err(io::Error::from_raw_os_error(err as i32)),
    }
}

/// Holds a per-user named mutex while the client runs. `None` if another
/// instance already holds it (e.g. the autostart copy).
pub struct SingleInstance(#[allow(dead_code)] windows_sys::Win32::Foundation::HANDLE);

impl SingleInstance {
    pub fn acquire() -> Option<Self> {
        // SAFETY: valid name; the handle is intentionally kept for the
        // process lifetime (released by the OS at exit).
        unsafe {
            let handle = CreateMutexW(ptr::null(), 0, wide(r"Local\onemouse-win").as_ptr());
            if handle.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
                None
            } else {
                Some(Self(handle))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quotes_like_the_windows_command_line_parser_expects() {
        assert_eq!(quote("--host"), "--host");
        assert_eq!(
            quote(r"C:\Program Files\onemouse.exe"),
            r#""C:\Program Files\onemouse.exe""#
        );
        assert_eq!(quote(""), r#""""#);
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }
}
