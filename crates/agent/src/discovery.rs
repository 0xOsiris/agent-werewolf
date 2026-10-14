//! Finds the game server on the LAN via mDNS.

use anyhow::{Context, bail};
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};
use reqwest::Client;
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tracing::warn;
use werewolf_protocol::{MDNS_SERVICE_TYPE, PROTOCOL_VERSION};

/// Per-address budget for checking that an advertised address reaches the server.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Returns the base URL of the first reachable, compatible server found within `timeout`.
pub async fn discover(timeout: Duration) -> anyhow::Result<String> {
    let daemon = ServiceDaemon::new().context("failed to start the mDNS daemon")?;
    let result = browse(&daemon, timeout).await;
    if let Err(error) = daemon.shutdown() {
        warn!(%error, "failed to shut down the mDNS daemon");
    }
    result
}

async fn browse(daemon: &ServiceDaemon, timeout: Duration) -> anyhow::Result<String> {
    let events = daemon
        .browse(MDNS_SERVICE_TYPE)
        .context("failed to start mDNS browsing")?;
    let http = Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .context("failed to build HTTP client")?;
    let mut unreachable = Vec::new();
    let search = async {
        while let Ok(event) = events.recv_async().await {
            let ServiceEvent::ServiceResolved(service) = event else {
                continue;
            };
            if !is_compatible(&service) {
                continue;
            }
            for addr in candidate_addrs(&service) {
                if unreachable.contains(&addr) {
                    continue;
                }
                if is_reachable(&http, addr).await {
                    return Some(format!("http://{addr}"));
                }
                unreachable.push(addr);
            }
        }
        None
    };
    let found = tokio::time::timeout(timeout, search).await;
    match found {
        Ok(Some(url)) => Ok(url),
        Ok(None) => bail!("mDNS browsing stopped before a server was found"),
        Err(_) if !unreachable.is_empty() => bail!(
            "found a werewolf server via mDNS at {unreachable:?} but could not connect; check the server machine's firewall, or pass --server http://<host>:7777"
        ),
        Err(_) => bail!(
            "no werewolf server found via mDNS within {}s. Venue Wi-Fi often blocks multicast; pass --server http://<host>:7777 instead",
            timeout.as_secs()
        ),
    }
}

fn is_compatible(service: &ResolvedService) -> bool {
    let version = service.get_property_val_str("version");
    if version == Some(PROTOCOL_VERSION.to_string().as_str()) {
        return true;
    }
    warn!(
        service = %service.fullname,
        version = version.unwrap_or("<missing>"),
        "ignoring server with an incompatible protocol version"
    );
    false
}

/// LAN IPv4 first; loopback covers a same-host server whose LAN address is firewalled.
fn candidate_addrs(service: &ResolvedService) -> Vec<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = service
        .get_addresses()
        .iter()
        .map(ScopedIp::to_ip_addr)
        // Link-local IPv6 needs a scope id that a URL can't carry.
        .filter(|ip| !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()))
        .map(|ip| SocketAddr::new(ip, service.get_port()))
        .collect();
    addrs.sort_by_key(|addr| (addr.is_ipv6(), addr.ip().is_loopback()));
    addrs
}

/// Advertised addresses can be unusable from here (VPNs, firewalls holding connections), so ask before trusting.
///
/// Any HTTP status counts: it proves the server itself answered.
async fn is_reachable(http: &Client, addr: SocketAddr) -> bool {
    match http.get(format!("http://{addr}/healthz")).send().await {
        Ok(_) => true,
        Err(error) => {
            warn!(%addr, %error, "advertised server address is unreachable");
            false
        }
    }
}
