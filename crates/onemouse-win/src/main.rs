use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use onemouse_protocol::Display;
use onemouse_transport::{IDENTITY_FILE, Identity, PEERS_FILE, PairingRequest, TrustStore};
use onemouse_win::client::{self, Config, Security, StaticHost};
use onemouse_win::inject::{Injector, LogBackend};
use onemouse_win::log;

const USAGE: &str = "\
onemouse-win: receive the Mac's keyboard and trackpad on this PC

USAGE:
    onemouse-win [--host <mac-ip>] [--port <port>] [--name <name>]
    onemouse-win --dry-run [--host <mac-ip>] [--fake-displays <layout>]
    onemouse-win --peers | --forget <name> | --list-displays

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
    -h, --help                Show this help

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
    ListDisplays,
    Help,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
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
fn confirm_in_terminal(req: &PairingRequest) -> bool {
    eprintln!();
    eprintln!(
        "  New Mac: \"{}\" (key {})",
        req.peer_name, req.peer_fingerprint
    );
    eprintln!();
    eprintln!("      Pairing code:  {}", req.code);
    eprintln!();
    eprintln!("  Check that the Mac shows the same code.");
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
        confirm: Box::new(confirm_in_terminal),
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
            match config_dir(run.config_dir.clone(), dry_run).and_then(|dir| security(&dir)) {
                Ok(security) => start(run, Arc::new(security)),
                Err(e) => Err(e),
            }
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn start(run: Run, security: Arc<Security>) -> io::Result<()> {
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
            client::run(
                &config,
                Arc::new(StaticHost { name, displays }),
                Arc::new(Mutex::new(Injector::new(LogBackend))),
            )
        }
        None => platform::run(&config, run.name),
    }
}

#[cfg(windows)]
mod platform {
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex, OnceLock};

    use onemouse_win::client::{self, Config, SharedInjector};
    use onemouse_win::inject::Injector;
    use onemouse_win::sendinput::SendInputBackend;
    use onemouse_win::{WindowsHost, display, log};
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    static INJECTOR: OnceLock<SharedInjector<SendInputBackend>> = OnceLock::new();

    /// Ctrl+C, closing the console, logoff, shutdown: release everything before
    /// the default handler kills the process.
    unsafe extern "system" fn on_console_event(_: u32) -> windows_sys::core::BOOL {
        if let Some(injector) = INJECTOR.get() {
            client::lock(injector).release_all();
        }
        0
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

    pub fn run(config: &Config, name: Option<String>) -> ! {
        display::enable_dpi_awareness();
        let injector = Arc::new(Mutex::new(Injector::new(SendInputBackend)));
        let _ = INJECTOR.set(Arc::clone(&injector));
        // SAFETY: registering a handler with the right signature.
        unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) };

        display::watch();
        let name = name
            .or_else(|| std::env::var("COMPUTERNAME").ok())
            .unwrap_or_else(|| "windows".into());
        log!("injecting input as {name}");
        client::run(config, Arc::new(WindowsHost { name }), injector)
    }
}

#[cfg(not(windows))]
mod platform {
    use std::process::ExitCode;

    use onemouse_win::client::Config;

    const ONLY_WINDOWS: &str =
        "onemouse-win injects input only on Windows; use --dry-run to test elsewhere";

    pub fn list_displays() -> ExitCode {
        eprintln!("{ONLY_WINDOWS}");
        ExitCode::FAILURE
    }

    pub fn run(_: &Config, _: Option<String>) -> ! {
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
