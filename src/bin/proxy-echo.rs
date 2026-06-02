//! `proxy-echo` — a tiny test backend for exercising sd-proxy's data path.
//!
//! It accepts a connection, decides the client's *real* source address, writes a single
//! report line back (`client=<ip:port> proxied=<bool>\n`), then echoes any further bytes.
//!
//! - If the peer speaks PROXY protocol (v1 or v2 — sd-proxy prepends a v2 header), the source
//!   is taken from that header: this is the address the *original* client connected from.
//! - Otherwise the source is the OS-level peer (`accept()`), which under rootless podman NAT is
//!   the masqueraded gateway — i.e. exactly the address sd-proxy exists to *avoid* leaking.
//!
//! So pointing a client at a `proxy`-mode port vs a directly-published port and comparing the
//! two report lines proves source-IP preservation end to end. Not published by the release CI;
//! built on demand (`just build-echo`, or cross-compiled for the e2e VM).

use std::net::SocketAddr;

use clap::Parser;
use eyre::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::signal;
use tracing::{debug, error, info};

#[derive(Parser)]
#[command(name = "proxy-echo")]
#[command(about = "Test backend: report the PROXY-protocol source (or the raw peer) back to the client")]
struct Args {
    /// Address to listen on.
    #[arg(long, env = "LISTEN", default_value = "0.0.0.0:25565")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "proxy_echo=info".into()),
        )
        .init();

    let args = Args::parse();
    let listener = TcpListener::bind(args.listen)
        .await
        .wrap_err("binding listener")?;
    info!(listen = %args.listen, "proxy-echo listening");

    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            _ = sigint.recv() => {
                info!("sigint received");
                break;
            }
            result = listener.accept() => match result {
                Ok((stream, peer)) => {
                    tokio::spawn(async move {
                        if let Err(err) = handle(stream, peer).await {
                            error!(?err, ?peer, "connection failed");
                        }
                    });
                }
                Err(err) => error!(?err, "failed to accept"),
            },
        }
    }
    Ok(())
}

async fn handle(mut stream: TcpStream, peer: SocketAddr) -> Result<()> {
    let (client, proxied) = detect_source(&mut stream, peer).await?;
    debug!(?client, proxied, ?peer, "resolved client source");

    let line = format!("client={client} proxied={proxied}\n");
    stream
        .write_all(line.as_bytes())
        .await
        .wrap_err("writing report line")?;
    stream.flush().await.ok();

    // Behave like a plain echo server for anything that follows.
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stream.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Read just enough to tell whether the peer led with a PROXY header. Returns the real client
/// source and whether it came from a PROXY header. Non-PROXY peers fall back to the OS peer.
async fn detect_source(stream: &mut TcpStream, peer: SocketAddr) -> Result<(SocketAddr, bool)> {
    use ppp::{HeaderResult, PartialResult};

    let mut buf = vec![0u8; 512];
    let mut read = 0;
    loop {
        let n = stream
            .read(&mut buf[read..])
            .await
            .wrap_err("reading PROXY header")?;
        if n == 0 {
            // Peer closed before we could decide: treat as a bare connection.
            return Ok((peer, false));
        }
        read += n;

        let result = HeaderResult::parse(&buf[..read]);
        match result {
            HeaderResult::V2(Ok(header)) => return Ok((source_of(header.addresses)?, true)),
            HeaderResult::V1(Ok(header)) => return Ok((v1_source_of(header.addresses)?, true)),
            // Complete but not a valid PROXY header → this is a bare (directly-published) client.
            other if other.is_complete() => return Ok((peer, false)),
            // Incomplete header: keep reading (bounded by the buffer).
            _ if read < buf.len() => continue,
            _ => bail!("PROXY header exceeded {} bytes", buf.len()),
        }
    }
}

fn source_of(addresses: ppp::v2::Addresses) -> Result<SocketAddr> {
    match addresses {
        ppp::v2::Addresses::IPv4(a) => Ok(SocketAddr::from((a.source_address, a.source_port))),
        ppp::v2::Addresses::IPv6(a) => Ok(SocketAddr::from((a.source_address, a.source_port))),
        other => bail!("unsupported PROXY v2 address family: {other:?}"),
    }
}

fn v1_source_of(addresses: ppp::v1::Addresses) -> Result<SocketAddr> {
    match addresses {
        ppp::v1::Addresses::Tcp4(a) => Ok(SocketAddr::from((a.source_address, a.source_port))),
        ppp::v1::Addresses::Tcp6(a) => Ok(SocketAddr::from((a.source_address, a.source_port))),
        ppp::v1::Addresses::Unknown => bail!("PROXY v1 header with unknown addresses"),
    }
}
