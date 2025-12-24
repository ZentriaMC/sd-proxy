use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use eyre::Context;

/// Target address that supports hostnames and IP addresses, with `__` separator support
#[derive(Debug, Clone)]
pub struct TargetAddr {
    pub host: String,
    pub port: u16,
}

impl FromStr for TargetAddr {
    type Err = eyre::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Replace `__` with `:` to support systemd unit instance definitions
        let normalized = s.replace("__", ":");

        // Parse hostname:port or IP:port
        let (host, port_str) = normalized.rsplit_once(':').ok_or_else(|| {
            eyre::eyre!("invalid address format: expected HOST:PORT or HOST__PORT")
        })?;

        let port = port_str
            .parse::<u16>()
            .map_err(|e| eyre::eyre!("invalid port: {}", e))?;

        Ok(TargetAddr {
            host: host.to_string(),
            port,
        })
    }
}

impl TargetAddr {
    /// Resolve the target address to a SocketAddr
    pub async fn resolve(&self) -> eyre::Result<SocketAddr> {
        let addr = format!("{}:{}", self.host, self.port);
        tokio::net::lookup_host(&addr)
            .await
            .wrap_err("failed to resolve hostname")?
            .next()
            .ok_or_else(|| eyre::eyre!("no addresses found for {}", addr))
    }
}

/// Normalize IPv4-mapped IPv6 addresses to IPv4
pub fn normalize_addr(addr: SocketAddr) -> SocketAddr {
    if let IpAddr::V6(ipv6) = addr.ip()
        && let Some(ipv4) = ipv6.to_ipv4_mapped()
    {
        SocketAddr::new(ipv4.into(), addr.port())
    } else {
        addr
    }
}
