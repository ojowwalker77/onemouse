//! End-to-end handshakes over loopback TCP.

use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use onemouse_protocol::{Message, read_message, write_message};
use onemouse_transport::{
    Error, Identity, Options, PairingRequest, Peer, SecureStream, TrustStore,
};

struct Side {
    identity: Identity,
    name: &'static str,
    trust: Mutex<TrustStore>,
    can_pair: bool,
    answer: bool,
    delay: Duration,
    seen: Mutex<Vec<PairingRequest>>,
}

impl Side {
    fn new(name: &'static str) -> Self {
        Self {
            identity: Identity::generate().unwrap(),
            name,
            trust: Mutex::new(TrustStore::in_memory()),
            can_pair: true,
            answer: true,
            delay: Duration::ZERO,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn pin(&self, other: &Side) {
        self.trust
            .lock()
            .unwrap()
            .pin(other.name, other.identity.public_key())
            .unwrap();
    }

    fn run(&self, stream: TcpStream, initiator: bool) -> Result<(SecureStream, Peer), Error> {
        let confirm = |req: &PairingRequest| {
            self.seen.lock().unwrap().push(req.clone());
            thread::sleep(self.delay);
            self.answer
        };
        let opts = Options {
            can_pair: self.can_pair,
            confirm: &confirm,
            pairing_timeout: Duration::from_millis(500),
            ..Options::new(&self.identity, self.name, &self.trust)
        };
        if initiator {
            onemouse_transport::connect(stream, &opts)
        } else {
            onemouse_transport::accept(stream, &opts)
        }
    }

    fn requests(&self) -> Vec<PairingRequest> {
        self.seen.lock().unwrap().clone()
    }
}

type Outcome = Result<(SecureStream, Peer), Error>;

/// `pc` connects to `mac` over loopback.
fn run(pc: &Side, mac: &Side) -> (Outcome, Outcome) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::scope(|s| {
        let server = s.spawn(|| mac.run(listener.accept().unwrap().0, false));
        let client = pc.run(TcpStream::connect(addr).unwrap(), true);
        (client, server.join().unwrap())
    })
}

#[test]
fn pinned_peers_connect_silently_and_exchange_messages() {
    let (pc, mac) = (Side::new("desk"), Side::new("macbook"));
    pc.pin(&mac);
    mac.pin(&pc);
    let (client, server) = run(&pc, &mac);
    let (mut client, mac_seen) = client.unwrap();
    let (server, pc_seen) = server.unwrap();
    assert_eq!(mac_seen.name, "macbook");
    assert_eq!(&mac_seen.key, mac.identity.public_key());
    assert!(!mac_seen.newly_paired);
    assert_eq!(pc_seen.fingerprint(), pc.identity.fingerprint());
    assert!(pc.requests().is_empty() && mac.requests().is_empty());

    // The existing framing runs unchanged on top, in both directions and
    // across reader/writer halves.
    let (mut reader, mut writer) = server.split();
    let big = Message::Reject {
        reason: "x".repeat(200_000),
    };
    write_message(&mut client, &Message::Ping(7)).unwrap();
    write_message(&mut client, &big).unwrap();
    assert_eq!(read_message(&mut reader).unwrap(), Message::Ping(7));
    assert_eq!(read_message(&mut reader).unwrap(), big);
    write_message(&mut writer, &Message::Pong(7)).unwrap();
    assert_eq!(read_message(&mut client).unwrap(), Message::Pong(7));
}

#[test]
fn first_connection_pairs_with_matching_codes_and_pins_both() {
    let (pc, mac) = (Side::new("desk"), Side::new("macbook"));
    let (client, server) = run(&pc, &mac);
    assert!(client.unwrap().1.newly_paired);
    assert!(server.unwrap().1.newly_paired);

    let (pc_req, mac_req) = (pc.requests(), mac.requests());
    assert_eq!(pc_req.len(), 1);
    assert_eq!(mac_req.len(), 1);
    assert_eq!(
        pc_req[0].code, mac_req[0].code,
        "both screens show the same code"
    );
    assert_eq!(pc_req[0].code.len(), 7);
    assert_eq!(pc_req[0].peer_name, "macbook");
    assert_eq!(pc_req[0].peer_fingerprint, mac.identity.fingerprint());
    assert_eq!(mac_req[0].peer_name, "desk");

    // Pinned now: the next connection is silent.
    let (client, server) = run(&pc, &mac);
    assert!(!client.unwrap().1.newly_paired);
    assert!(!server.unwrap().1.newly_paired);
    assert_eq!(pc.requests().len(), 1);
}

#[test]
fn codes_differ_between_sessions() {
    let (pc, mac) = (Side::new("desk"), Side::new("macbook"));
    let mut mac_no = Side::new("macbook");
    mac_no.answer = false;
    let _ = run(&pc, &mac_no);
    let _ = run(&pc, &mac);
    let codes: Vec<_> = pc.requests().into_iter().map(|r| r.code).collect();
    assert_eq!(codes.len(), 2);
    assert_ne!(codes[0], codes[1]);
}

#[test]
fn declined_on_either_side_pins_nothing() {
    for decline_on_pc in [true, false] {
        let (mut pc, mut mac) = (Side::new("desk"), Side::new("macbook"));
        if decline_on_pc {
            pc.answer = false;
        } else {
            mac.answer = false;
        }
        let (client, server) = run(&pc, &mac);
        let (client, server) = (client.unwrap_err(), server.unwrap_err());
        assert!(
            matches!(client, Error::Declined { by_peer } if by_peer != decline_on_pc),
            "{client:?}"
        );
        assert!(
            matches!(server, Error::Declined { by_peer } if by_peer == decline_on_pc),
            "{server:?}"
        );
        assert!(pc.trust.lock().unwrap().peers().is_empty());
        assert!(mac.trust.lock().unwrap().peers().is_empty());
    }
}

#[test]
fn unknown_peer_is_refused_when_the_mac_is_not_pairing() {
    let (pc, mut mac) = (Side::new("desk"), Side::new("macbook"));
    mac.can_pair = false;
    let (client, server) = run(&pc, &mac);
    assert!(matches!(
        client.unwrap_err(),
        Error::NotPairing { here: false }
    ));
    assert!(matches!(
        server.unwrap_err(),
        Error::NotPairing { here: true }
    ));
    assert!(pc.requests().is_empty() && mac.requests().is_empty());
}

#[test]
fn pinned_name_with_a_different_key_fails_hard() {
    let (pc, mac) = (Side::new("desk"), Side::new("macbook"));
    let impostor = Side::new("macbook");
    pc.pin(&mac);
    impostor.pin(&pc);
    let (client, server) = run(&pc, &impostor);
    assert!(matches!(client.unwrap_err(), Error::KeyChanged { ref name } if name == "macbook"));
    assert!(matches!(server.unwrap_err(), Error::PeerKeyChanged { .. }));
    assert!(pc.requests().is_empty(), "never offers to re-pair");
    assert_eq!(
        pc.trust.lock().unwrap().peers()[0].key,
        *mac.identity.public_key(),
        "old key kept"
    );
}

/// The mac says "I know you" (trust 0), honestly or not: the PC hasn't
/// pinned it, so the PC's own user must still confirm. The peer's trust byte
/// never loosens our decision.
#[test]
fn peer_claiming_trust_does_not_skip_our_confirmation() {
    let (pc, mac) = (Side::new("desk"), Side::new("macbook"));
    mac.pin(&pc);
    let (client, server) = run(&pc, &mac);
    client.unwrap();
    server.unwrap();
    assert_eq!(pc.requests().len(), 1);
    assert_eq!(mac.requests().len(), 1);

    // Same with our user declining: nothing gets pinned on our side.
    let (mut pc, mac) = (Side::new("desk"), Side::new("macbook"));
    pc.answer = false;
    mac.pin(&pc);
    let (client, _) = run(&pc, &mac);
    assert!(matches!(
        client.unwrap_err(),
        Error::Declined { by_peer: false }
    ));
    assert!(pc.trust.lock().unwrap().peers().is_empty());
}

#[test]
fn renamed_peer_with_a_known_key_connects_and_updates_the_name() {
    let (pc, mac) = (Side::new("desk"), Side::new("macbook"));
    pc.pin(&mac);
    mac.pin(&pc);
    let paired_at = pc.trust.lock().unwrap().peers()[0].paired_at;
    let renamed = Side {
        identity: mac.identity,
        trust: mac.trust,
        ..Side::new("jow's MacBook Air")
    };
    let (client, server) = run(&pc, &renamed);
    assert_eq!(client.unwrap().1.name, "jow's MacBook Air");
    server.unwrap();
    let peers = pc.trust.lock().unwrap().peers().to_vec();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].name, "jow's MacBook Air");
    assert_eq!(peers[0].paired_at, paired_at);
}

#[test]
fn slow_confirmation_times_out() {
    let (pc, mut mac) = (Side::new("desk"), Side::new("macbook"));
    // Well past the 500 ms limit: Windows receive timeouts can overshoot.
    mac.delay = Duration::from_secs(2);
    let (client, server) = run(&pc, &mac);
    let client = client.unwrap_err();
    assert!(matches!(client, Error::PairingTimeout), "{client:?}");
    assert!(matches!(
        server.unwrap_err(),
        Error::Declined { by_peer: false } | Error::Io(_)
    ));
    assert!(pc.trust.lock().unwrap().peers().is_empty());
    assert!(mac.trust.lock().unwrap().peers().is_empty());
}

#[test]
fn garbage_instead_of_a_handshake_is_rejected() {
    let mac = Side::new("macbook");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        use std::io::Write;
        let mut s = TcpStream::connect(addr).unwrap();
        // A plaintext v1 Hello frame.
        s.write_all(&[6, 0, 0, 0, 0, 1, 1, b'w', 1, 0]).unwrap();
        thread::sleep(Duration::from_millis(200));
    });
    let err = mac.run(listener.accept().unwrap().0, false).unwrap_err();
    assert!(matches!(err, Error::Noise(_) | Error::Io(_)), "{err:?}");
    client.join().unwrap();
}
