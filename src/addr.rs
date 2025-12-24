use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use eyre::{Context, bail};

use tracing::debug;

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
    /// Resolve the target address to a list of SocketAddrs filtered to match the client's IP version.
    /// If client is IPv4, only returns IPv4 addresses; if IPv6, only returns IPv6 addresses.
    pub async fn resolve(&self, client_addr: Option<SocketAddr>) -> eyre::Result<Vec<SocketAddr>> {
        let addr = format!("{}:{}", self.host, self.port);
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host(&addr)
            .await
            .wrap_err("failed to resolve hostname")?
            .collect();

        if addresses.is_empty() {
            bail!("no addresses found for {}", addr);
        }

        // If client address is not supplied, return any address
        let Some(client_addr) = client_addr else {
            return Ok(addresses);
        };

        let client_is_ipv6 = client_addr.ip().is_ipv6();
        debug!(
            client_is_ipv6,
            "matching target ip address version with client"
        );

        // Filter addresses to match client's IP version
        let filtered: Vec<SocketAddr> = addresses
            .into_iter()
            .filter(|addr| addr.ip().is_ipv6() == client_is_ipv6)
            .collect();

        if filtered.is_empty() {
            let ip_version_str = if client_is_ipv6 { "IPv6" } else { "IPv4" };
            bail!(
                "no {} addresses found for target {} (client is {})",
                ip_version_str,
                self.host,
                ip_version_str
            );
        }

        Ok(filtered)
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
