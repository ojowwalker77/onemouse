// No console window on Windows: it runs in the background with a tray icon.
// CLI commands still print when started from a terminal (see `attach_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use onemouse_protocol::Display;
use onemouse_transport::{IDENTITY_FILE, Identity, PEERS_FILE, PairingRequest, TrustStore};
use onemouse_win::client::{self, Config, Role, Security, StaticHost};
use onemouse_win::inject::{Injector, LogBackend};
use onemouse_win::log;

const USAGE: &str = "\
onemouse-win: receive the Mac's keyboard and trackpad on this PC

USAGE:
    onemouse-win [--host <mac-ip>] [--port <port>] [--name <name>]
    onemouse-win --dry-run [--host <mac-ip>] [--fake-displays <layout>]
    onemouse-win --install [--host <mac-ip>] [...]   start at every login
    onemouse-win --uninstall | --peers | --forget <name> | --list-displays

OPTIONS:
    --host <mac-ip>           Address of the Mac (default: find it on the network)
    --port <port>             TCP port with --host (default 24801)
    --name <name>             Name shown on the Mac (default: this computer's name)
    --config-dir <dir>        Where the key and paired devices are kept
                              (default %APPDATA%\\onemouse; --dry-run uses a
                              separate \"dry-run\" subfolder)
    --dry-run                 Log the input instead of injecting it. Runs on any
                              OS, so the Mac side can be tested without a PC
    --fake-displays <layout>  Displays to report in --dry-run, comma-separated
                              WxH:X:Y[@scale], first is primary
                              (default 1920x1080:0:0@1)
    --peers                   Show this device's key and the paired Macs
    --forget <name>           Unpair a Mac (e.g. after it was reinstalled)
    --list-displays           Print the displays that would be reported, then exit
    --install                 Start onemouse at every login with the other options
                              given (Windows), then exit
    --uninstall               Stop starting at login (Windows)
    -h, --help                Show this help

On Windows it runs in the background: look for the icon in the notification
area (Quit, Open log). The log is onemouse-win.log in the config folder.

The connection is encrypted. The first time, both screens show a 6-digit
code: pair only if they match.";

#[derive(Debug, PartialEq)]
struct Run {
    host: Option<String>,
    port: u16,
    name: Option<String>,
    /// `Some` in `--dry-run`.
    dry_run: Option<Vec<Display>>,
    config_dir: Option<PathBuf>,
}

#[derive(Debug, PartialEq)]
enum Command {
    Run(Run),
    Peers {
        config_dir: Option<PathBuf>,
    },
    Forget {
        name: String,
        config_dir: Option<PathBuf>,
    },
    /// Run at login with these arguments.
    Install(Vec<String>),
    Uninstall,
    ListDisplays,
    Help,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let args: Vec<String> = args.into_iter().collect();
    if args.iter().any(|a| a == "--uninstall") {
        return match args.as_slice() {
            [_] => Ok(Command::Uninstall),
            _ => Err("--uninstall takes no other options".into()),
        };
    }
    if args.iter().any(|a| a == "--install") {
        let rest: Vec<String> = args.into_iter().filter(|a| a != "--install").collect();
        return match parse(rest.clone())? {
            Command::Run(Run { dry_run: None, .. }) => Ok(Command::Install(rest)),
            _ => Err("--install goes with the options of a normal run (e.g. --host)".into()),
        };
    }
    let mut args = args.into_iter();
    let (mut host, mut port, mut name) = (None, onemouse_protocol::DEFAULT_PORT, None);
    let (mut list, mut dry_run, mut fake) = (false, false, None);
    let (mut peers, mut forget, mut config_dir) = (false, None, None);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "--host" => host = Some(value("--host")?),
            "--port" => {
                let v = value("--port")?;
                port = v.parse().map_err(|_| format!("invalid port: {v}"))?;
            }
            "--name" => name = Some(value("--name")?),
            "--config-dir" => config_dir = Some(PathBuf::from(value("--config-dir")?)),
            "--dry-run" => dry_run = true,
            "--fake-displays" => fake = Some(parse_displays(&value("--fake-displays")?)?),
            "--peers" => peers = true,
            "--forget" => forget = Some(value("--forget")?),
            "--list-displays" => list = true,
            "-h" | "--help" => return Ok(Command::Help),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    if list {
        return Ok(Command::ListDisplays);
    }
    if peers {
        return Ok(Command::Peers { config_dir });
    }
    if let Some(name) = forget {
        return Ok(Command::Forget { name, config_dir });
    }
    if fake.is_some() && !dry_run {
        return Err("--fake-displays needs --dry-run".into());
    }
    let dry_run = dry_run.then(|| fake.unwrap_or_else(|| parse_displays("1920x1080:0:0").unwrap()));
    Ok(Command::Run(Run {
        host,
        port,
        name,
        dry_run,
        config_dir,
    }))
}

/// `WxH:X:Y[@scale]`, comma-separated; the first display is the primary.
fn parse_displays(spec: &str) -> Result<Vec<Display>, String> {
    let parse_one = |(i, item): (usize, &str)| -> Option<Display> {
        let (geometry, scale) = match item.split_once('@') {
            Some((g, s)) => (g, s.parse().ok().filter(|s: &f32| *s > 0.0)?),
            None => (item, 1.0),
        };
        let mut parts = geometry.split(':');
        let (w, h) = parts.next()?.split_once('x')?;
        let display = Display {
            id: i as u32 + 1,
            x: parts.next()?.parse().ok()?,
            y: parts.next()?.parse().ok()?,
            width: w.parse().ok().filter(|w| *w > 0)?,
            height: h.parse().ok().filter(|h| *h > 0)?,
            scale,
            primary: i == 0,
        };
        parts.next().is_none().then_some(display)
    };
    spec.split(',')
        .map(str::trim)
        .enumerate()
        .map(|(i, item)| {
            parse_one((i, item)).ok_or(format!(
                "invalid display {item:?}, expected WxH:X:Y[@scale] like 2560x1440:-2560:0@2"
            ))
        })
        .collect()
}

fn config_dir(explicit: Option<PathBuf>, dry_run: bool) -> io::Result<PathBuf> {
    match explicit {
        Some(dir) => Ok(dir),
        None if dry_run => Ok(onemouse_transport::config_dir()?.join("dry-run")),
        None => onemouse_transport::config_dir(),
    }
}

fn load_trust(dir: &Path) -> io::Result<TrustStore> {
    TrustStore::load(&dir.join(PEERS_FILE))
}

/// Shows the pairing code in the terminal and asks the user to compare.
#[cfg_attr(windows, allow(dead_code))]
fn confirm_in_terminal(req: &PairingRequest) -> bool {
    eprintln!();
    eprintln!(
        "  New Mac: \"{}\" (key {})",
        req.peer_name, req.peer_fingerprint
    );
    eprintln!();
    eprintln!("      Pairing code:  {}", req.code);
    eprintln!();
    let secs = req
        .deadline
        .saturating_duration_since(std::time::Instant::now())
        .as_secs();
    eprintln!("  Check that the Mac shows the same code (within {secs} s).");
    eprint!("  Type y and press Enter if it matches (anything else cancels): ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    match io::stdin().lock().read_line(&mut answer) {
        Ok(n) if n > 0 => matches!(answer.trim(), "y" | "Y" | "yes" | "s" | "sim"),
        _ => false,
    }
}

fn security(dir: &Path) -> io::Result<Security> {
    Ok(Security {
        identity: Identity::load_or_create(&dir.join(IDENTITY_FILE))?,
        trust: Mutex::new(load_trust(dir)?),
        confirm: Box::new(platform::confirm),
    })
}

fn days_ago(unix: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    now.saturating_sub(unix) / 86_400
}

fn peers(dir: &Path) -> io::Result<()> {
    let identity = Identity::load_or_create(&dir.join(IDENTITY_FILE))?;
    println!("this PC's key: {}", identity.fingerprint());
    println!("stored in:     {}", dir.display());
    let trust = load_trust(dir)?;
    if trust.peers().is_empty() {
        println!("no paired Macs yet");
    }
    for peer in trust.peers() {
        println!(
            "paired: {}  (key {}, {} day(s) ago)",
            peer.name,
            onemouse_transport::fingerprint(&peer.key),
            days_ago(peer.paired_at)
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    platform::attach_console();
    let command = match parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match command {
        Command::Help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Command::ListDisplays => return platform::list_displays(),
        Command::Install(args) => platform::install(&args),
        Command::Uninstall => platform::uninstall(),
        Command::Peers { config_dir: dir } => config_dir(dir, false).and_then(|dir| peers(&dir)),
        Command::Forget {
            name,
            config_dir: dir,
        } => config_dir(dir, false).and_then(|dir| {
            if load_trust(&dir)?.forget(&name)? {
                println!("forgot {name}");
            } else {
                println!("no paired Mac named {name:?} (see --peers)");
            }
            Ok(())
        }),
        Command::Run(run) => {
            let dry_run = run.dry_run.is_some();
            config_dir(run.config_dir.clone(), dry_run).and_then(|dir| {
                if let Err(e) = onemouse_win::logging::to_file(&dir.join(LOG_FILE)) {
                    eprintln!("can't write the log file: {e}");
                }
                let security = security(&dir)?;
                start(run, Arc::new(security), &dir)
            })
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log!("error: {e}");
            platform::fatal(&e.to_string());
            ExitCode::FAILURE
        }
    }
}

const LOG_FILE: &str = "onemouse-win.log";

fn start(run: Run, security: Arc<Security>, dir: &Path) -> io::Result<()> {
    log!("this PC's key: {}", security.identity.fingerprint());
    match &run.host {
        Some(host) => log!("connecting to {host}:{}", run.port),
        None => log!("looking for the Mac on the network"),
    }
    let config = Config::new(run.host, run.port, security);
    match run.dry_run {
        Some(displays) => {
            let name = run.name.unwrap_or_else(|| "onemouse-dry-run".into());
            log!("dry run as {name}: logging input instead of injecting it");
            let host = Arc::new(StaticHost { name, displays });
            // No hooks off Windows (and never in a dry run): the role
            // transitions still work, the PC just never goes remote.
            let role = Arc::new(Role::new(&host, false));
            client::run(
                &config,
                host,
                Arc::new(Mutex::new(Injector::new(LogBackend))),
                &role,
            )
        }
        None => platform::run(&config, run.name, &dir.join(LOG_FILE)),
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::process::ExitCode;
    use std::ptr;
    use std::sync::{Arc, Mutex, OnceLock};

    use onemouse_transport::PairingRequest;
    use onemouse_win::autostart::{self, SingleInstance};
    use onemouse_win::client::{self, Config, Host, SharedInjector};
    use onemouse_win::inject::Injector;
    use onemouse_win::sendinput::SendInputBackend;
    use onemouse_win::{WindowsHost, display, log, tray};
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IDNO, IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONQUESTION, MB_OK,
        MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW, PostMessageW, WM_COMMAND,
    };

    static INJECTOR: OnceLock<SharedInjector<SendInputBackend>> = OnceLock::new();

    fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(Some(0)).collect()
    }

    fn release_all() {
        if let Some(injector) = INJECTOR.get() {
            client::lock(injector).release_all();
        }
    }

    /// Ctrl+C / Ctrl+Break are ignored: the Mac's Cmd+C arrives as Ctrl+C
    /// and must not stop us if our own console has focus. Closing the
    /// console, logoff, shutdown: release everything, then let it end.
    unsafe extern "system" fn on_console_event(event: u32) -> windows_sys::core::BOOL {
        if matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
            return 1;
        }
        release_all();
        0
    }

    /// The windowless build has no console of its own; borrow the terminal
    /// it was started from, if any, so CLI commands can print.
    pub fn attach_console() {
        // SAFETY: plain Win32 call; failing just means no terminal.
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }

    fn message_box(title: &str, text: &str, flags: u32) -> i32 {
        // SAFETY: valid NUL-terminated strings; no owner window.
        unsafe {
            MessageBoxW(
                ptr::null_mut(),
                wide(text).as_ptr(),
                wide(title).as_ptr(),
                flags | MB_TOPMOST | MB_SETFOREGROUND,
            )
        }
    }

    /// Pairing confirmation as a dialog: the windowless build has no
    /// console to type into.
    pub fn confirm(req: &PairingRequest) -> bool {
        let secs = req
            .deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_secs();
        let text = format!(
            "Pair this PC with the Mac \"{}\"?\n\n\
             Pairing code:   {}\n\n\
             Click Yes only if the Mac shows exactly the same code \
             (within {secs} s). If the codes differ, click No.\n\n\
             Mac key: {}",
            req.peer_name, req.code, req.peer_fingerprint
        );
        // Unique title, so we can find the box to close it at the deadline.
        let title = format!("onemouse: pair with {}?", req.code);
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let title = title.clone();
            // No is the default button: a stray Enter typed from the Mac while
            // the box pops up must not approve a pairing.
            std::thread::spawn(move || {
                let flags = MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2;
                let _ = tx.send(message_box(&title, &text, flags) == IDYES);
            });
        }
        let left = req
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(answer) => answer,
            Err(_) => {
                // Too late anyway: close the box as "No".
                // SAFETY: plain window lookup and a posted message.
                unsafe {
                    let hwnd = FindWindowW(wide("#32770").as_ptr(), wide(&title).as_ptr());
                    if !hwnd.is_null() {
                        PostMessageW(hwnd, WM_COMMAND, IDNO as usize, 0);
                    }
                }
                false
            }
        }
    }

    /// Errors that end the program get a dialog too: nobody sees stderr.
    pub fn fatal(message: &str) {
        message_box(
            "onemouse",
            &format!("onemouse stopped:\n\n{message}"),
            MB_OK | MB_ICONERROR,
        );
    }

    pub fn install(args: &[String]) -> io::Result<()> {
        let command = autostart::install(args)?;
        println!("onemouse will start at every login:\n  {command}");
        println!("Start it now with the same command, or log out and back in.");
        Ok(())
    }

    pub fn uninstall() -> io::Result<()> {
        if autostart::uninstall()? {
            println!("onemouse won't start at login anymore");
        } else {
            println!("onemouse wasn't set to start at login");
        }
        Ok(())
    }

    pub fn list_displays() -> ExitCode {
        display::enable_dpi_awareness();
        for d in display::displays() {
            println!(
                "{:>10}  {}x{} at ({}, {})  scale {}{}",
                d.id,
                d.width,
                d.height,
                d.x,
                d.y,
                d.scale,
                if d.primary { "  primary" } else { "" }
            );
        }
        ExitCode::SUCCESS
    }

    pub fn run(config: &Config, name: Option<String>, log_path: &Path) -> ! {
        let Some(_instance) = SingleInstance::acquire() else {
            log!("onemouse is already running on this PC; exiting");
            std::process::exit(0);
        };
        display::enable_dpi_awareness();
        let injector = Arc::new(Mutex::new(Injector::new(SendInputBackend)));
        let _ = INJECTOR.set(Arc::clone(&injector));
        // SAFETY: registering a handler with the right signature.
        unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) };
        tray::start(log_path.to_owned(), release_all);

        display::watch();
        let name = name
            .or_else(|| std::env::var("COMPUTERNAME").ok())
            .unwrap_or_else(|| "windows".into());
        let host = Arc::new(WindowsHost {
            name,
            config_path: log_path.parent().map(|dir| dir.join("arrangement")),
        });
        let role = Arc::new(client::Role::new(&host, true));
        tray::set_role(Arc::clone(&role));
        log!("injecting input as {}", host.name());
        client::run(config, host, injector, &role)
    }
}

#[cfg(not(windows))]
mod platform {
    use std::process::ExitCode;

    use onemouse_win::client::Config;

    const ONLY_WINDOWS: &str =
        "onemouse-win injects input only on Windows; use --dry-run to test elsewhere";

    pub fn attach_console() {}

    pub fn confirm(req: &onemouse_transport::PairingRequest) -> bool {
        super::confirm_in_terminal(req)
    }

    pub fn fatal(_: &str) {}

    pub fn install(_: &[String]) -> std::io::Result<()> {
        Err(std::io::Error::other("--install is Windows-only"))
    }

    pub fn uninstall() -> std::io::Result<()> {
        Err(std::io::Error::other("--uninstall is Windows-only"))
    }

    pub fn list_displays() -> ExitCode {
        eprintln!("{ONLY_WINDOWS}");
        ExitCode::FAILURE
    }

    pub fn run(_: &Config, _: Option<String>, _: &std::path::Path) -> ! {
        eprintln!("{ONLY_WINDOWS}");
        std::process::exit(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn run(host: &str, port: u16, name: Option<&str>, dry_run: Option<Vec<Display>>) -> Command {
        Command::Run(Run {
            host: Some(host.into()),
            port,
            name: name.map(Into::into),
            dry_run,
            config_dir: None,
        })
    }

    fn display(id: u32, x: i32, y: i32, w: u32, h: u32, scale: f32) -> Display {
        Display {
            id,
            x,
            y,
            width: w,
            height: h,
            scale,
            primary: id == 1,
        }
    }

    #[test]
    fn parses_host_and_defaults() {
        let port = onemouse_protocol::DEFAULT_PORT;
        assert_eq!(
            parse(args("--host 192.168.1.20")),
            Ok(run("192.168.1.20", port, None, None))
        );
        assert_eq!(
            parse(args("--host mac.local --port 9000 --name desk")),
            Ok(run("mac.local", 9000, Some("desk"), None))
        );
    }

    #[test]
    fn parses_dry_run() {
        let port = onemouse_protocol::DEFAULT_PORT;
        assert_eq!(
            parse(args("--dry-run --host 127.0.0.1")),
            Ok(run(
                "127.0.0.1",
                port,
                None,
                Some(vec![display(1, 0, 0, 1920, 1080, 1.0)])
            ))
        );
        assert_eq!(
            parse(args(
                "--host h --dry-run --fake-displays 1920x1080:0:0@1.25,2560x1440:-2560:-200@2"
            )),
            Ok(run(
                "h",
                port,
                None,
                Some(vec![
                    display(1, 0, 0, 1920, 1080, 1.25),
                    display(2, -2560, -200, 2560, 1440, 2.0),
                ])
            ))
        );
    }

    #[test]
    fn parses_discovery_and_peer_management() {
        assert_eq!(
            parse(args("")),
            Ok(Command::Run(Run {
                host: None,
                port: onemouse_protocol::DEFAULT_PORT,
                name: None,
                dry_run: None,
                config_dir: None,
            }))
        );
        assert_eq!(
            parse(args("--peers --config-dir /tmp/om")),
            Ok(Command::Peers {
                config_dir: Some("/tmp/om".into())
            })
        );
        assert_eq!(
            parse(args("--forget MacBook")),
            Ok(Command::Forget {
                name: "MacBook".into(),
                config_dir: None
            })
        );
        assert!(parse(args("--forget")).is_err());
    }

    #[test]
    fn dry_run_keeps_its_own_identity() {
        let real = config_dir(None, false).unwrap();
        let dry = config_dir(None, true).unwrap();
        assert_ne!(real, dry);
        assert!(dry.starts_with(&real));
        assert_eq!(
            config_dir(Some("/x".into()), true).unwrap(),
            PathBuf::from("/x")
        );
    }

    #[test]
    fn parses_install_and_uninstall() {
        assert_eq!(
            parse(args("--install --host 192.168.0.51")),
            Ok(Command::Install(args("--host 192.168.0.51")))
        );
        assert_eq!(parse(args("--install")), Ok(Command::Install(vec![])));
        assert_eq!(parse(args("--uninstall")), Ok(Command::Uninstall));
        assert!(parse(args("--uninstall --host x")).is_err());
        assert!(parse(args("--install --dry-run")).is_err());
        assert!(parse(args("--install --peers")).is_err());
        assert!(parse(args("--install --bogus")).is_err());
    }

    #[test]
    fn rejects_bad_args() {
        assert!(parse(args("--host")).is_err());
        assert!(parse(args("--host a --port x")).is_err());
        assert!(parse(args("--bogus")).is_err());
        assert!(parse(args("--host a --fake-displays 10x10:0:0")).is_err());
        for bad in [
            "10x10",
            "10x10:0",
            "10x10:0:0:0",
            "0x10:0:0",
            "10x10:0:0@0",
            "10x10:0:0@x",
            "axb:0:0",
        ] {
            assert!(parse_displays(bad).is_err(), "{bad}");
        }
        assert_eq!(parse(args("--list-displays")), Ok(Command::ListDisplays));
        assert_eq!(parse(args("--host a -h")), Ok(Command::Help));
    }
}
