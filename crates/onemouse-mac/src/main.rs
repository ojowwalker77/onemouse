use std::process::ExitCode;

use onemouse_mac::layout::Side;

const USAGE: &str = "\
onemouse-mac: share this Mac's keyboard and trackpad with a Windows PC

USAGE:
    onemouse-mac [--side <side>] [--arrange] [--port <port>] [--name <name>] [--scroll-speed <x>]
    onemouse-mac --list-displays

OPTIONS:
    --side <side>        Where the PC starts until you arrange it: left, right, top or
                         bottom (default right)
    --arrange            Open the Arrange Displays window at launch
    --port <port>        TCP port to listen on (default 24801)
    --name <name>        Name shown on the PC (default: this Mac's name)
    --scroll-speed <x>   Scroll speed multiplier on the PC (default 1.0)
    --list-displays      Print this Mac's displays, then exit
    -h, --help           Show this help

Arrange the PC from the ⇄ menu-bar item, then push the cursor where the two
screens touch to control the PC; push it back to return.
Ctrl+Option+Cmd+Esc always brings the cursor back to the Mac.

The connection is encrypted. A new PC must be paired once: choose \"Pair a
New PC…\" in the ⇄ menu and confirm that both screens show the same code.";

#[derive(Debug, PartialEq)]
enum Command {
    Run {
        side: Side,
        port: u16,
        name: Option<String>,
        scroll_speed: f64,
        arrange: bool,
    },
    ListDisplays,
    Help,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let (mut side, mut port, mut name) = (Side::Right, onemouse_protocol::DEFAULT_PORT, None);
    let mut scroll_speed = 1.0;
    let (mut list, mut arrange) = (false, false);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "--side" => {
                let v = value("--side")?;
                side = Side::parse(&v).ok_or(format!("invalid side: {v}"))?;
            }
            "--port" => {
                let v = value("--port")?;
                port = v.parse().map_err(|_| format!("invalid port: {v}"))?;
            }
            "--name" => name = Some(value("--name")?),
            "--scroll-speed" => {
                let v = value("--scroll-speed")?;
                scroll_speed = v
                    .parse()
                    .ok()
                    .filter(|s: &f64| *s > 0.0)
                    .ok_or(format!("invalid scroll speed: {v}"))?;
            }
            "--arrange" => arrange = true,
            "--list-displays" => list = true,
            "-h" | "--help" => return Ok(Command::Help),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    if list {
        return Ok(Command::ListDisplays);
    }
    Ok(Command::Run {
        side,
        port,
        name,
        scroll_speed,
        arrange,
    })
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

#[cfg(target_os = "macos")]
mod platform {
    use std::net::{TcpListener, UdpSocket};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::Instant;

    use onemouse_mac::config::Config;
    use onemouse_mac::controller::Controller;
    use onemouse_mac::pairing::Prompts;
    use onemouse_mac::server::{self, Link, Security};
    use onemouse_mac::{log, macos};
    use onemouse_transport::{IDENTITY_FILE, Identity, PEERS_FILE, TrustStore, discovery};

    use super::Command;

    pub fn run(command: Command) -> ExitCode {
        let Command::Run {
            side,
            port,
            name,
            scroll_speed,
            arrange,
        } = command
        else {
            for d in macos::displays() {
                println!("{}x{} at ({}, {})", d.width, d.height, d.x, d.y);
            }
            return ExitCode::SUCCESS;
        };

        if !macos::ensure_permissions() {
            log!(
                "needs Accessibility and Input Monitoring: allow this terminal in \
                 System Settings → Privacy & Security, then run again"
            );
            return ExitCode::FAILURE;
        }

        let listener = match TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => l,
            Err(e) => {
                log!("can't listen on port {port}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let security = match load_security() {
            Ok(s) => s,
            Err(e) => {
                log!("can't load this Mac's key or paired PCs: {e}");
                return ExitCode::FAILURE;
            }
        };
        let prompts = Arc::new(Prompts::default());
        let security = Arc::new(Security::new(security.0, security.1, {
            let prompts = Arc::clone(&prompts);
            Box::new(move |req| {
                let left = req.deadline.saturating_duration_since(Instant::now());
                prompts.ask(req, left)
            })
        }));

        let name = name.unwrap_or_else(computer_name);
        let link = Link::new();
        server::serve(
            listener,
            link.clone(),
            server::Config::new(name.clone(), Arc::clone(&security)),
        );
        let fingerprint = security.identity.fingerprint();
        // Dropping it unregisters; it lives as long as the app.
        let _advertisement = discovery::advertise(&name, &fingerprint, port)
            .inspect_err(|e| log!("not advertising on the network (use --host on the PC): {e}"))
            .ok();

        log!(
            "{name} listening on {}:{port}, key {fingerprint}",
            lan_ip().unwrap_or_else(|| "0.0.0.0".into())
        );
        log!("Ctrl+Option+Cmd+Esc brings the cursor back");
        let config_path = onemouse_transport::config_dir()
            .ok()
            .map(|dir| dir.join("arrangement"));
        let config = config_path.as_deref().map(Config::load).unwrap_or_default();
        log!("arrange the PC from the ⇄ menu-bar item");
        let controller = Controller::new(side, config.origin);
        let app = macos::App {
            link,
            security,
            prompts,
            config,
            config_path,
            scroll_speed,
            arrange_at_start: arrange,
        };
        match macos::run(controller, app) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                log!("{e}");
                ExitCode::FAILURE
            }
        }
    }

    /// This Mac's key and the PCs paired with it, in
    /// `~/Library/Application Support/onemouse`.
    fn load_security() -> std::io::Result<(Identity, TrustStore)> {
        let dir = onemouse_transport::config_dir()?;
        Ok((
            Identity::load_or_create(&dir.join(IDENTITY_FILE))?,
            TrustStore::load(&dir.join(PEERS_FILE))?,
        ))
    }

    fn computer_name() -> String {
        std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "mac".into())
    }

    /// The address other machines reach us on (no packets are sent).
    fn lan_ip() -> Option<String> {
        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        Some(socket.local_addr().ok()?.ip().to_string())
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::process::ExitCode;

    pub fn run(_: super::Command) -> ExitCode {
        eprintln!("onemouse-mac only runs on macOS");
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
    fn parses_options_and_defaults() {
        assert_eq!(
            parse(args("")),
            Ok(Command::Run {
                side: Side::Right,
                port: onemouse_protocol::DEFAULT_PORT,
                name: None,
                scroll_speed: 1.0,
                arrange: false,
            })
        );
        assert_eq!(
            parse(args(
                "--side below --port 9000 --name air --scroll-speed 2.5 --arrange"
            )),
            Ok(Command::Run {
                side: Side::Bottom,
                port: 9000,
                name: Some("air".into()),
                scroll_speed: 2.5,
                arrange: true,
            })
        );
    }

    #[test]
    fn rejects_bad_args() {
        assert!(parse(args("--side up")).is_err());
        assert!(parse(args("--port x")).is_err());
        assert!(parse(args("--scroll-speed 0")).is_err());
        assert!(parse(args("--bogus")).is_err());
        assert_eq!(parse(args("--list-displays")), Ok(Command::ListDisplays));
        assert_eq!(parse(args("--side left -h")), Ok(Command::Help));
    }
}
