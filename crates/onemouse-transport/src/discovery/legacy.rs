//! One-shot ("legacy unicast", RFC 6762 §6.7) DNS-SD queries.
//!
//! The query leaves from an ephemeral port instead of 5353, so responders
//! answer by unicast to that port. That reply is ordinary return traffic:
//! it gets through a host firewall that drops unsolicited inbound multicast
//! (Windows on a Public network), and no other mDNS stack on the host
//! (Windows' `Dnscache` also binds 5353) competes for it. macOS'
//! mDNSResponder and `mdns-sd` both answer such queries, with the SRV, TXT
//! and address records in the same message.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use super::Found;

const GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 251), 5353);
/// Answers can be lost on Wi-Fi: ask again this often.
const RETRY: Duration = Duration::from_secs(1);
/// How long one receive waits before checking `stop` and the other sockets.
const POLL: Duration = Duration::from_millis(50);

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;

/// Queries `service` (e.g. `_onemouse._tcp.local.`) on every usable IPv4
/// interface until `deadline` or `stop`, sending each service found to `out`
/// (repeats included: the caller dedupes).
pub(super) fn browse(service: &str, deadline: Instant, stop: &AtomicBool, out: &Sender<Found>) {
    let query = ptr_query(service);
    let sockets: Vec<UdpSocket> = interfaces()
        .into_iter()
        .filter_map(|ip| socket(ip).ok())
        .collect();
    if sockets.is_empty() {
        return;
    }
    let mut buf = vec![0u8; 9000];
    let mut next_send = Instant::now();
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        if Instant::now() >= next_send {
            for s in &sockets {
                let _ = s.send_to(&query, GROUP);
            }
            next_send += RETRY;
        }
        for s in &sockets {
            match s.recv_from(&mut buf) {
                Ok((n, _)) => {
                    for found in parse_response(&buf[..n], service) {
                        if out.send(found).is_err() {
                            return;
                        }
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => {}
            }
        }
    }
}

/// Up, non-loopback, non-link-local IPv4 addresses.
fn interfaces() -> Vec<Ipv4Addr> {
    let Ok(ifs) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut ips: Vec<Ipv4Addr> = ifs
        .into_iter()
        .filter_map(|i| match i.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() => Some(ip),
            _ => None,
        })
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

/// A socket on `ip`, ephemeral port, sending multicast out of `ip`'s interface.
fn socket(ip: Ipv4Addr) -> std::io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.bind(&SocketAddrV4::new(ip, 0).into())?;
    s.set_multicast_if_v4(&ip)?;
    s.set_multicast_ttl_v4(255)?;
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(POLL))?;
    Ok(s)
}

/// A standard query (ID nonzero, as legacy queriers must) for `name` PTR.
fn ptr_query(name: &str) -> Vec<u8> {
    let mut q = vec![0x6f, 0x6d, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.trim_end_matches('.').split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&TYPE_PTR.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes()); // IN
    q
}

struct Record<'a> {
    name: String,
    rtype: u16,
    rdata: &'a [u8],
    /// Offset of `rdata` in the message, for names compressed inside it.
    at: usize,
}

/// Every complete service instance of `service` in a DNS response: PTR to
/// an instance with SRV and a TXT `fp`; addresses from the SRV target's A
/// and AAAA records (possibly none). Malformed messages yield nothing.
pub(super) fn parse_response(msg: &[u8], service: &str) -> Vec<Found> {
    parse_records(msg)
        .map(|records| collect(msg, &records, service))
        .unwrap_or_default()
}

fn collect(msg: &[u8], records: &[Record], service: &str) -> Vec<Found> {
    let service = service.trim_end_matches('.');
    let mut found = Vec::new();
    for ptr in records
        .iter()
        .filter(|r| r.rtype == TYPE_PTR && r.name.eq_ignore_ascii_case(service))
    {
        let Some((instance, _)) = read_name(msg, ptr.at) else {
            continue;
        };
        let instance = instance.as_str();
        let of = |t: u16| {
            records
                .iter()
                .filter(move |r| r.rtype == t && r.name.eq_ignore_ascii_case(instance))
        };
        let Some((port, target)) = of(TYPE_SRV).find_map(|r| srv(msg, r)) else {
            continue;
        };
        let txt = of(TYPE_TXT)
            .next()
            .map(|r| txt(r.rdata))
            .unwrap_or_default();
        let get = |k: &str| txt.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        let Some(fingerprint) = get("fp") else {
            continue;
        };
        let mut addrs: Vec<IpAddr> = records
            .iter()
            .filter(|r| r.name.eq_ignore_ascii_case(&target))
            .filter_map(|r| match (r.rtype, r.rdata.len()) {
                (TYPE_A, 4) => Some(IpAddr::V4(Ipv4Addr::from(
                    <[u8; 4]>::try_from(r.rdata).ok()?,
                ))),
                (TYPE_AAAA, 16) => Some(IpAddr::V6(Ipv6Addr::from(
                    <[u8; 16]>::try_from(r.rdata).ok()?,
                ))),
                _ => None,
            })
            .collect();
        addrs.sort_by_key(|a| (a.is_ipv6(), *a));
        addrs.dedup();
        found.push(Found {
            name: get("name").unwrap_or_else(|| instance.to_owned()),
            version: get("v").and_then(|v| v.parse().ok()),
            fingerprint,
            addrs,
            port,
        });
    }
    found
}

fn parse_records(msg: &[u8]) -> Option<Vec<Record<'_>>> {
    let u16_at = |i: usize| Some(u16::from_be_bytes([*msg.get(i)?, *msg.get(i + 1)?]));
    if u16_at(2)? & 0x8000 == 0 {
        return None; // a query, not a response
    }
    let questions = u16_at(4)?;
    let records = [u16_at(6)?, u16_at(8)?, u16_at(10)?]
        .iter()
        .map(|&n| usize::from(n))
        .sum::<usize>();
    let mut at = 12;
    for _ in 0..questions {
        at = read_name(msg, at)?.1 + 4;
    }
    let mut out = Vec::with_capacity(records);
    for _ in 0..records {
        let (name, end) = read_name(msg, at)?;
        let rtype = u16_at(end)?;
        let len = usize::from(u16_at(end + 8)?);
        let start = end + 10;
        let rdata = msg.get(start..start + len)?;
        out.push(Record {
            name,
            rtype,
            rdata,
            at: start,
        });
        at = start + len;
    }
    Some(out)
}

/// SRV rdata: priority, weight, port, target.
fn srv(msg: &[u8], r: &Record) -> Option<(u16, String)> {
    let port = u16::from_be_bytes([*r.rdata.get(4)?, *r.rdata.get(5)?]);
    let (target, _) = read_name(msg, r.at + 6)?;
    Some((port, target))
}

/// TXT rdata: length-prefixed `key=value` strings.
fn txt(rdata: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(&len) = rdata.get(at) {
        let Some(s) = rdata.get(at + 1..at + 1 + usize::from(len)) else {
            break;
        };
        let s = String::from_utf8_lossy(s);
        if let Some((k, v)) = s.split_once('=') {
            out.push((k.to_ascii_lowercase(), v.to_owned()));
        }
        at += 1 + usize::from(len);
    }
    out
}

/// The (possibly compressed) name at `at`, dot-separated without the final
/// dot, and the offset just past it.
fn read_name(msg: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    // Every pointer must go backwards, so this terminates; the cap is cheap
    // insurance.
    for _ in 0..128 {
        let len = *msg.get(at)?;
        match len {
            0 => {
                return Some((labels.join("."), end.unwrap_or(at + 1)));
            }
            l if l & 0xc0 == 0xc0 => {
                let target = usize::from(u16::from_be_bytes([l & 0x3f, *msg.get(at + 1)?]));
                if target >= at {
                    return None;
                }
                end.get_or_insert(at + 2);
                at = target;
            }
            l if l & 0xc0 == 0 => {
                let label = msg.get(at + 1..at + 1 + usize::from(l))?;
                labels.push(String::from_utf8_lossy(label).into_owned());
                at += 1 + usize::from(l);
            }
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// A real answer from macOS' mDNSResponder (`dns-sd -R`) to
    /// `ptr_query("_onemouse._tcp.local.")`, captured on the Windows PC
    /// (#12).
    const MAC_ANSWER: &str = "123484000001000100000006095F6F6E656D6F757365045F746370056C6F63616C00000C0001C00C000C00010000000A001F1C6A6F772773204D6163426F6F6B2041697220286B6977697465737429C00CC032002100010000000A00190000000060E1106A6F77732D4D6163426F6F6B2D416972C01BC032001000010000000A002B0F66703D6B6977697465737430303030166E616D653D6A6F772773204D6163426F6F6B2041697203763D33C063001C00010000000A0010FE80000000000000189C6CBAA64F1F39C063001C00010000000A00102804014C04C79BB014B2DE392A804FC6C063000100010000000A0004C0A80006C063001C00010000000A00102804014C04C79BB00000000000001006";

    #[test]
    fn parses_a_real_mac_answer() {
        let found = parse_response(&hex(MAC_ANSWER), "_onemouse._tcp.local.");
        assert_eq!(found.len(), 1);
        let mac = &found[0];
        assert_eq!(mac.name, "jow's MacBook Air");
        assert_eq!(mac.fingerprint, "kiwitest0000");
        assert_eq!(mac.version, Some(3));
        assert_eq!(mac.port, 24801);
        assert_eq!(mac.addrs[0], "192.168.0.6".parse::<IpAddr>().unwrap());
        assert_eq!(mac.addrs.len(), 4, "{:?}", mac.addrs);
    }

    #[test]
    fn query_is_a_ptr_question() {
        let q = ptr_query("_onemouse._tcp.local.");
        assert_eq!(&q[4..6], &[0, 1]);
        assert_eq!(read_name(&q, 12).unwrap().0, "_onemouse._tcp.local");
        assert_eq!(&q[q.len() - 4..], &[0, 12, 0, 1]);
    }

    #[test]
    fn other_services_and_queries_are_ignored() {
        let msg = hex(MAC_ANSWER);
        assert!(parse_response(&msg, "_other._tcp.local.").is_empty());
        assert!(
            parse_response(&ptr_query("_onemouse._tcp.local."), "_onemouse._tcp.local.").is_empty()
        );
    }

    #[test]
    fn truncated_or_looping_messages_yield_nothing() {
        let msg = hex(MAC_ANSWER);
        for n in 0..msg.len() {
            parse_response(&msg[..n], "_onemouse._tcp.local.");
        }
        // A name pointing at itself.
        let mut looping = msg.clone();
        looping[12] = 0xc0;
        looping[13] = 12;
        assert!(parse_response(&looping, "_onemouse._tcp.local.").is_empty());
    }

    /// Needs multicast on the host; CI runners often don't have it.
    #[test]
    #[ignore = "needs a network with multicast"]
    fn mdns_sd_advertiser_answers() {
        let fp = "fedcba9876543210";
        let _ad = super::super::advertise("legacy-test", fp, 24802).unwrap();
        std::thread::sleep(Duration::from_secs(2)); // probing
        let (tx, rx) = std::sync::mpsc::channel();
        let deadline = Instant::now() + Duration::from_secs(4);
        browse(
            onemouse_protocol::SERVICE_TYPE,
            deadline,
            &AtomicBool::new(false),
            &tx,
        );
        drop(tx);
        let found: Vec<Found> = rx.iter().filter(|f| f.fingerprint == fp).collect();
        let ad = found.first().expect("answered");
        assert_eq!(ad.name, "legacy-test");
        assert_eq!(ad.port, 24802);
        assert!(!ad.addrs.is_empty());
    }
}
