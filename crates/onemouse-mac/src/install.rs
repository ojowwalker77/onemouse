//! Running at login without a terminal: a per-user LaunchAgent that starts
//! a stable copy of the binary, restarts it if it crashes (not after Quit),
//! and sends its log to `~/Library/Logs/onemouse.log`.

use std::path::{Path, PathBuf};

pub const LABEL: &str = "com.onemouse.mac";

/// Where an installed onemouse lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The copy the LaunchAgent runs. Accessibility and Input Monitoring are
    /// granted to this file.
    pub exe: PathBuf,
    pub plist: PathBuf,
    pub log: PathBuf,
}

impl Paths {
    pub fn under(home: &Path) -> Self {
        Self {
            exe: home.join("Library/Application Support/onemouse/onemouse-mac"),
            plist: home.join(format!("Library/LaunchAgents/{LABEL}.plist")),
            log: home.join("Library/Logs/onemouse.log"),
        }
    }

    pub fn for_current_user() -> Option<Self> {
        std::env::var_os("HOME").map(|home| Self::under(Path::new(&home)))
    }
}

/// The LaunchAgent: run `exe args…` at login, keep it alive unless it quit
/// cleanly, log to `log`.
pub fn plist(paths: &Paths, args: &[String]) -> String {
    let program = std::iter::once(paths.exe.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|a| format!("        <string>{}</string>\n", escape(&a)))
        .collect::<String>();
    let log = escape(&paths.log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{program}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_live_in_the_users_library() {
        let p = Paths::under(Path::new("/Users/jow"));
        assert_eq!(
            p.exe,
            Path::new("/Users/jow/Library/Application Support/onemouse/onemouse-mac")
        );
        assert_eq!(
            p.plist,
            Path::new("/Users/jow/Library/LaunchAgents/com.onemouse.mac.plist")
        );
        assert_eq!(p.log, Path::new("/Users/jow/Library/Logs/onemouse.log"));
    }

    #[test]
    fn plist_runs_the_installed_copy_with_escaped_args() {
        let p = Paths::under(Path::new("/Users/jow"));
        let xml = plist(&p, &["--name".into(), "jow's <Mac> & co".into()]);
        assert!(xml.contains(
            "<string>/Users/jow/Library/Application Support/onemouse/onemouse-mac</string>\n        <string>--name</string>\n        <string>jow's &lt;Mac&gt; &amp; co</string>"
        ));
        assert!(xml.contains("<key>SuccessfulExit</key>\n        <false/>"));
        assert!(xml.contains("<string>/Users/jow/Library/Logs/onemouse.log</string>"));
        assert!(xml.contains("<string>com.onemouse.mac</string>"));
    }

    /// launchd rejects malformed plists silently; let macOS's own parser check.
    #[cfg(target_os = "macos")]
    #[test]
    fn plist_is_valid_for_macos() {
        let p = Paths::under(Path::new("/Users/jow"));
        let file = std::env::temp_dir().join(format!("onemouse-{}.plist", std::process::id()));
        std::fs::write(&file, plist(&p, &["--name".into(), "a & b".into()])).unwrap();
        let lint = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&file)
            .output()
            .unwrap();
        std::fs::remove_file(&file).unwrap();
        assert!(
            lint.status.success(),
            "{}",
            String::from_utf8_lossy(&lint.stdout)
        );
    }
}
