//! Public URLs and the play-page QR code, fixed at startup.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::Context;
use qrcode::{QrCode, render::svg};
use tracing::warn;

use crate::config::Config;

pub struct Site {
    /// Base URL without a trailing slash, e.g. `http://10.0.0.5:7777`.
    pub base_url: String,
    pub play_url: String,
    pub qr_svg: String,
}

impl Site {
    pub fn new(config: &Config, listen: SocketAddr) -> anyhow::Result<Self> {
        let base_url = match &config.public_url {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => {
                let ip = if listen.ip().is_unspecified() {
                    lan_ipv4()
                } else {
                    listen.ip()
                };
                format!("http://{}", SocketAddr::new(ip, listen.port()))
            }
        };
        let play_url = format!("{base_url}/play");
        let qr_svg = QrCode::new(play_url.as_bytes())
            .context("encoding the play URL as a QR code")?
            .render::<svg::Color<'_>>()
            .min_dimensions(320, 320)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .quiet_zone(true)
            .build();
        Ok(Self {
            base_url,
            play_url,
            qr_svg,
        })
    }
}

/// First non-loopback, non-link-local IPv4, so phones on the LAN can reach us.
fn lan_ipv4() -> IpAddr {
    let found = if_addrs::get_if_addrs().map(|interfaces| {
        interfaces
            .into_iter()
            .map(|i| i.ip())
            .find(|ip| matches!(ip, IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local()))
    });
    match found {
        Ok(Some(ip)) => ip,
        Ok(None) => {
            warn!("no LAN IPv4 address found; advertising 127.0.0.1 (set --public-url)");
            Ipv4Addr::LOCALHOST.into()
        }
        Err(err) => {
            warn!(%err, "listing network interfaces failed; advertising 127.0.0.1 (set --public-url)");
            Ipv4Addr::LOCALHOST.into()
        }
    }
}
