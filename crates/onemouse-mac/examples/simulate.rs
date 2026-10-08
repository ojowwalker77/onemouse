//! Scripted session through the real controller and listener, without the
//! event tap: run `onemouse-win --dry-run --host 127.0.0.1` alongside and
//! compare its log. Doesn't touch this Mac's input.

use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use onemouse_mac::controller::{Controller, Input, Peer};
use onemouse_mac::layout::{Point, Rect, Side};
use onemouse_mac::server::{self, Link};
use onemouse_protocol::key;
use onemouse_protocol::{DEFAULT_PORT, MouseButton};

fn main() {
    let listener = TcpListener::bind(("127.0.0.1", DEFAULT_PORT)).expect("port 24801 busy");
    let link = Link::new();
    server::serve(listener, link.clone(), server::Config::new("simulated-mac"));
    eprintln!("waiting for onemouse-win…");
    while !link.with_peer(|p| p.is_some()) {
        thread::sleep(Duration::from_millis(100));
    }

    let mac = [Rect::new(0.0, 0.0, 1470.0, 956.0)];
    let edge = Point::new(1469.0, 478.0);
    let mv = |dx, dy| Input::Move { pos: edge, dx, dy };
    let key = |code, pressed| Input::Key { code, pressed };
    let script = [
        ("typing on the Mac stays local", key(key::A, true)),
        ("", key(key::A, false)),
        ("Cmd held while crossing", key(key::LEFT_META, true)),
        ("cross the right edge", mv(3.0, 0.0)),
        ("move on the PC", mv(40.0, -20.0)),
        ("Cmd+C", key(key::C, true)),
        ("", key(key::C, false)),
        ("", key(key::LEFT_META, false)),
        (
            "click",
            Input::Button {
                button: MouseButton::Left,
                pressed: true,
            },
        ),
        (
            "",
            Input::Button {
                button: MouseButton::Left,
                pressed: false,
            },
        ),
        (
            "scroll down a notch",
            Input::Scroll {
                dx: 0.0,
                dy: -120.0,
            },
        ),
        ("hold Shift then leave", key(key::LEFT_SHIFT, true)),
        ("move back across", mv(-500.0, 0.0)),
        ("", key(key::LEFT_SHIFT, false)),
    ];
    let mut controller = Controller::new(Side::Right, None);
    for (label, input) in script {
        let out = link.with_peer(|peer| {
            let view = peer.map(|p| Peer {
                id: p.id(),
                displays: &p.displays,
            });
            let mut out = controller.handle(input, &mac, view);
            for msg in &out.send {
                peer.unwrap().send(msg.clone());
            }
            std::mem::take(&mut out.send)
        });
        if !label.is_empty() {
            eprintln!("--- {label}");
        }
        eprintln!("    sent {out:?}");
        thread::sleep(Duration::from_millis(150));
    }
    thread::sleep(Duration::from_millis(500));
}
