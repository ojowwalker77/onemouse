//! mDNS: the primary advertises [`SERVICE_TYPE`] with its name and key
//! fingerprint, the secondary browses for it.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use onemouse_protocol::{PROTOCOL_VERSION, SERVICE_TYPE};

pub use mdns_sd::Error;

/// A primary seen on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    /// `PROTOCOL_VERSION` it speaks (TXT `v`).
    pub version: Option<u16>,
    pub fingerprint: String,
    pub addrs: Vec<IpAddr>,
    pub port: u16,
}

/// Keeps the service advertised until dropped.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        if let Ok(done) = self.daemon.unregister(&self.fullname) {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
        let _ = self.daemon.shutdown();
    }
}

/// Advertises this primary on all interfaces.
pub fn advertise(name: &str, fingerprint: &str, port: u16) -> Result<Advertisement, Error> {
    let daemon = ServiceDaemon::new()?;
    let host = format!("onemouse-{fingerprint}.local.");
    let instance = format!("{name} ({})", &fingerprint[..fingerprint.len().min(8)]);
    let version = PROTOCOL_VERSION.to_string();
    let props = [("fp", fingerprint), ("name", name), ("v", &version)];
    let info =
        ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", port, &props[..])?.enable_addr_auto();
    let fullname = info.get_fullname().to_owned();
    daemon.register(info)?;
    Ok(Advertisement { daemon, fullname })
}

/// Browses for primaries for up to `timeout`. Returns early as soon as one
/// matches `want` (a fingerprint) and speaks our protocol version. Callers
/// should skip entries whose `version` isn't ours.
pub fn browse(timeout: Duration, want: Option<&str>) -> Result<Vec<Found>, Error> {
    let daemon = ServiceDaemon::new()?;
    let events = daemon.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + timeout;
    let mut found: HashMap<String, Found> = HashMap::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = events.recv_timeout(left) else {
            break;
        };
        if let ServiceEvent::ServiceResolved(service) = event {
            let Some(fp) = service.get_property_val_str("fp") else {
                continue;
            };
            let entry = Found {
                name: service
                    .get_property_val_str("name")
                    .unwrap_or(&service.fullname)
                    .to_owned(),
                version: service
                    .get_property_val_str("v")
                    .and_then(|v| v.parse().ok()),
                fingerprint: fp.to_owned(),
                addrs: {
                    let mut addrs: Vec<_> =
                        service.addresses.iter().map(|a| a.to_ip_addr()).collect();
                    // IPv4 first: link-local IPv6 needs a scope to connect.
                    addrs.sort_by_key(|a| (a.is_ipv6(), *a));
                    addrs
                },
                port: service.port,
            };
            let matched = want == Some(fp) && entry.version == Some(PROTOCOL_VERSION);
            found.insert(service.fullname.clone(), entry);
            if matched {
                break;
            }
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    let mut found: Vec<_> = found.into_values().collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs multicast on the host; CI runners often don't have it.
    #[test]
    #[ignore = "needs a network with multicast"]
    fn advertised_service_is_found() {
        let fp = "0123456789abcdef";
        let _ad = advertise("test-mac", fp, 24801).unwrap();
        let found = browse(Duration::from_secs(5), Some(fp)).unwrap();
        let mac = found.iter().find(|f| f.fingerprint == fp).expect("found");
        assert_eq!(mac.name, "test-mac");
        assert_eq!(mac.port, 24801);
        assert_eq!(mac.version, Some(PROTOCOL_VERSION));
        assert!(!mac.addrs.is_empty());
    }
}
