use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use onemouse_protocol::Display;
use onemouse_win::client::{self, Config, StaticHost};
use onemouse_win::inject::{Injector, LogBackend};
use onemouse_win::log;

const USAGE: &str = "\
onemouse-win: receive the Mac's keyboard and trackpad on this PC

USAGE:
    onemouse-win --host <mac-ip> [--port <port>] [--name <name>]
    onemouse-win --dry-run --host <mac-ip> [--fake-displays <layout>]
    onemouse-win --list-displays

OPTIONS:
    --host <mac-ip>           Address of the Mac running onemouse
    --port <port>             TCP port (default 24801)
    --name <name>             Name shown on the Mac (default: this computer's name)
    --dry-run                 Log the input instead of injecting it. Runs on any
                              OS, so the Mac side can be tested without a PC
    --fake-displays <layout>  Displays to report in --dry-run, comma-separated
                              WxH:X:Y[@scale], first is primary
                              (default 1920x1080:0:0@1)
    --list-displays           Print the displays that would be reported, then exit
    -h, --help                Show this help

v1 is plaintext: use it only on a trusted LAN until encryption (M2) lands.";

#[derive(Debug, PartialEq)]
struct Run {
    host: String,
    port: u16,
    name: Option<String>,
    /// `Some` in `--dry-run`.
    dry_run: Option<Vec<Display>>,
}

#[derive(Debug, PartialEq)]
enum Command {
    Run(Run),
    ListDisplays,
    Help,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let (mut host, mut port, mut name) = (None, onemouse_protocol::DEFAULT_PORT, None);
    let (mut list, mut dry_run, mut fake) = (false, false, None);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "--host" => host = Some(value("--host")?),
            "--port" => {
                let v = value("--port")?;
                port = v.parse().map_err(|_| format!("invalid port: {v}"))?;
            }
            "--name" => name = Some(value("--name")?),
            "--dry-run" => dry_run = true,
            "--fake-displays" => fake = Some(parse_displays(&value("--fake-displays")?)?),
            "--list-displays" => list = true,
            "-h" | "--help" => return Ok(Command::Help),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    if list {
        return Ok(Command::ListDisplays);
    }
    if fake.is_some() && !dry_run {
        return Err("--fake-displays needs --dry-run".into());
    }
    let host = host.ok_or("--host is required")?;
    let dry_run = dry_run.then(|| fake.unwrap_or_else(|| parse_displays("1920x1080:0:0").unwrap()));
    Ok(Command::Run(Run {
        host,
        port,
        name,
        dry_run,
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

fn main() -> ExitCode {
    let command = match parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Help => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Command::Run(Run {
            host,
            port,
            name,
            dry_run: Some(displays),
        }) => {
            let name = name.unwrap_or_else(|| "onemouse-dry-run".into());
            log!("dry run: logging input instead of injecting it");
            log!("connecting to {host}:{port} as {name}");
            client::run(
                &Config::new(host, port),
                Arc::new(StaticHost { name, displays }),
                Arc::new(Mutex::new(Injector::new(LogBackend))),
            )
        }
        command => platform::run(command),
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

    use super::{Command, Run};

    static INJECTOR: OnceLock<SharedInjector<SendInputBackend>> = OnceLock::new();

    /// Ctrl+C, closing the console, logoff, shutdown: release everything before
    /// the default handler kills the process.
    unsafe extern "system" fn on_console_event(_: u32) -> windows_sys::core::BOOL {
        if let Some(injector) = INJECTOR.get() {
            client::lock(injector).release_all();
        }
        0
    }

    pub fn run(command: Command) -> ExitCode {
        display::enable_dpi_awareness();
        let Command::Run(Run {
            host, port, name, ..
        }) = command
        else {
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
            return ExitCode::SUCCESS;
        };

        let injector = Arc::new(Mutex::new(Injector::new(SendInputBackend)));
        let _ = INJECTOR.set(Arc::clone(&injector));
        // SAFETY: registering a handler with the right signature.
        unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) };

        display::watch();
        let name = name
            .or_else(|| std::env::var("COMPUTERNAME").ok())
            .unwrap_or_else(|| "windows".into());
        log!("onemouse-win: plaintext v1 protocol, trusted LAN only");
        log!("connecting to {host}:{port} as {name}");
        client::run(
            &Config::new(host, port),
            Arc::new(WindowsHost { name }),
            injector,
        )
    }
}

#[cfg(not(windows))]
mod platform {
    use std::process::ExitCode;

    pub fn run(_: super::Command) -> ExitCode {
        eprintln!("onemouse-win injects input only on Windows; use --dry-run to test elsewhere");
        ExitCode::FAILURE
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
            host: host.into(),
            port,
            name: name.map(Into::into),
            dry_run,
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
    fn rejects_bad_args() {
        assert!(parse(args("")).is_err());
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
