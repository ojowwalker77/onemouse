//! Log lines to stderr and, once [`to_file`] was called, to a log file, so a
//! background (windowless) client still leaves a trail.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static FILE: Mutex<Option<File>> = Mutex::new(None);

/// Logs files bigger than this are started over on the next launch.
const MAX_LOG_LEN: u64 = 1 << 20;

/// Appends future log lines to `path` (starting it over if it got big).
pub fn to_file(path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let too_big = fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_LEN);
    let file = OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(true)
        .truncate(too_big)
        .open(path)?;
    *FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
    Ok(())
}

#[doc(hidden)]
pub fn write(args: fmt::Arguments) {
    let line = format!("{} [onemouse-win] {args}", timestamp());
    // Without a console (windowless build) stderr is a no-op sink.
    eprintln!("{line}");
    if let Some(file) = FILE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = writeln!(file, "{line}");
    }
}

/// `HH:MM:SS` UTC; enough to line logs up with the Mac's.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}Z", day / 3600, day % 3600 / 60, day % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_go_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("onemouse-win.log");
        to_file(&path).unwrap();
        crate::log!("hello {}", 42);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[onemouse-win] hello 42"), "{text}");
        assert!(text.ends_with('\n'));
        *FILE.lock().unwrap() = None;
    }
}
