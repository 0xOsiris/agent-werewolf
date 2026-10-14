//! LAN discovery: advertises the server as `_werewolf._tcp.local.`.

use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceInfo};
use tracing::warn;
use werewolf_protocol::{MDNS_SERVICE_TYPE, PROTOCOL_VERSION};

/// How long to wait for the goodbye packet on shutdown.
const UNREGISTER_TIMEOUT: Duration = Duration::from_secs(1);

pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

/// Registers the service; on failure warns and returns `None`, since agents can use the URL.
pub fn advertise(port: u16) -> Option<Advertisement> {
    try_advertise(port)
        .inspect_err(
            |err| warn!(%err, "mDNS registration failed; agents must use the URL directly"),
        )
        .ok()
}

fn try_advertise(port: u16) -> mdns_sd::Result<Advertisement> {
    let hostname = gethostname::gethostname();
    let label: String = hostname
        .to_string_lossy()
        .split('.')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let label = if label.is_empty() {
        "server".to_owned()
    } else {
        label
    };
    let version = PROTOCOL_VERSION.to_string();
    let info = ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        &format!("werewolf-{label}"),
        &format!("{label}.local."),
        (),
        port,
        &[("version", version.as_str())][..],
    )?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_owned();
    let daemon = ServiceDaemon::new()?;
    daemon.register(info)?;
    Ok(Advertisement { daemon, fullname })
}

impl Advertisement {
    /// Unregisters (sending the goodbye packet) and stops the daemon.
    pub async fn withdraw(self) {
        let result = tokio::task::spawn_blocking(move || -> mdns_sd::Result<()> {
            let status = self.daemon.unregister(&self.fullname)?;
            if let Err(err) = status.recv_timeout(UNREGISTER_TIMEOUT) {
                warn!(%err, "mDNS unregister not confirmed");
            }
            self.daemon.shutdown()?;
            Ok(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(%err, "mDNS unregister failed"),
            Err(err) => warn!(%err, "mDNS unregister task failed"),
        }
    }
}
