use std::process::ExitCode;

const USAGE: &str = "\
onemouse-win: receive the Mac's keyboard and trackpad on this PC

USAGE:
    onemouse-win --host <mac-ip> [--port <port>] [--name <name>]
    onemouse-win --list-displays

OPTIONS:
    --host <mac-ip>    Address of the Mac running onemouse
    --port <port>      TCP port (default 24801)
    --name <name>      Name shown on the Mac (default: this computer's name)
    --list-displays    Print the displays that would be reported, then exit
    -h, --help         Show this help

v1 is plaintext: use it only on a trusted LAN until encryption (M2) lands.";

#[derive(Debug, PartialEq)]
enum Command {
    Run {
        host: String,
        port: u16,
        name: Option<String>,
    },
    ListDisplays,
    Help,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let (mut host, mut port, mut name) = (None, onemouse_protocol::DEFAULT_PORT, None);
    let mut list = false;
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "--host" => host = Some(value("--host")?),
            "--port" => {
                let v = value("--port")?;
                port = v.parse().map_err(|_| format!("invalid port: {v}"))?;
            }
            "--name" => name = Some(value("--name")?),
            "--list-displays" => list = true,
            "-h" | "--help" => return Ok(Command::Help),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    if list {
        return Ok(Command::ListDisplays);
    }
    match host {
        Some(host) => Ok(Command::Run { host, port, name }),
        None => Err("--host is required".into()),
    }
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

    use super::Command;

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
        let Command::Run { host, port, name } = command else {
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
        eprintln!("onemouse-win only runs on Windows");
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_host_and_defaults() {
        assert_eq!(
            parse(args("--host 192.168.1.20")),
            Ok(Command::Run {
                host: "192.168.1.20".into(),
                port: onemouse_protocol::DEFAULT_PORT,
                name: None,
            })
        );
        assert_eq!(
            parse(args("--host mac.local --port 9000 --name desk")),
            Ok(Command::Run {
                host: "mac.local".into(),
                port: 9000,
                name: Some("desk".into()),
            })
        );
    }

    #[test]
    fn rejects_bad_args() {
        assert!(parse(args("")).is_err());
        assert!(parse(args("--host")).is_err());
        assert!(parse(args("--host a --port x")).is_err());
        assert!(parse(args("--bogus")).is_err());
        assert_eq!(parse(args("--list-displays")), Ok(Command::ListDisplays));
        assert_eq!(parse(args("--host a -h")), Ok(Command::Help));
    }
}
