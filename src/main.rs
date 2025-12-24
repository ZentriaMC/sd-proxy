use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use eyre::{Context, ContextCompat, Result};
use listenfd::ListenFd;
use ppp::v2;
use tokio::io::AsyncWriteExt;
use tokio::signal;
use tokio_tfo::{TfoListener, TfoStream};
use tracing::{debug, error, info, trace};

mod addr;

use crate::addr::{TargetAddr, normalize_addr};

async fn get_listener() -> Result<TfoListener> {
    let mut listenfd = ListenFd::from_env();

    let listener = listenfd
        .take_tcp_listener(0)
        .wrap_err("failed to get listener")?
        .wrap_err("listener not available")?;

    Ok(TfoListener::from_std(listener)?)
}

async fn handle_client(
    client_stream: TfoStream,
    client_addr: SocketAddr,
    target_addr: Arc<TargetAddr>,
) -> Result<()> {
    debug!(?client_addr, "new inbound connection");

    client_stream.set_nodelay(true)?;

    let resolved_target = target_addr
        .resolve()
        .await
        .wrap_err("failed to resolve target address")?;

    debug!(?resolved_target, ?target_addr.host, ?target_addr.port, "resolved target");

    // Normalize IPv4-mapped IPv6 addresses to IPv4
    let normalized_client = normalize_addr(client_addr);
    let normalized_target = normalize_addr(resolved_target);

    let proxy_header = v2::Builder::with_addresses(
        v2::Version::Two | v2::Command::Proxy,
        v2::Protocol::Stream,
        (normalized_client, normalized_target),
    )
    .build()
    .wrap_err("failed to build PROXY protocol v2 header")?;

    let mut target_stream = TfoStream::connect(resolved_target)
        .await
        .with_context(|| format!("failed to connect to target {}", resolved_target))?;

    target_stream.set_nodelay(true)?;

    debug!(?resolved_target, ?client_addr, "connection established");

    // In case TFO worked, this will be sent in the SYN packet
    target_stream
        .write_all(&proxy_header)
        .await
        .wrap_err("failed to write PROXY protocol header")?;

    trace!(
        ?normalized_client,
        ?normalized_target,
        "sent PROXY protocol v2 header",
    );

    let (mut client_read, mut client_write) = client_stream.split();
    let (mut target_read, mut target_write) = target_stream.split();

    let client_to_target = async move {
        let result = tokio::io::copy(&mut client_read, &mut target_write).await;
        if let Err(ref err) = result {
            error!(?err, "error copying client -> target");
        }
        result
    };

    let target_to_client = async move {
        let result = tokio::io::copy(&mut target_read, &mut client_write).await;
        if let Err(ref err) = result {
            error!(?err, "error copying target -> client");
        }
        result
    };

    tokio::select! {
        result = client_to_target => {
            if let Ok(bytes) = result {
                trace!(?bytes, "client -> target done");
            }
        }
        result = target_to_client => {
            if let Ok(bytes) = result {
                trace!(?bytes, "target -> client done");
            }
        }
    }

    debug!(?normalized_client, ?normalized_target, "connection closed");
    Ok(())
}

#[derive(Parser)]
#[command(name = "sd-proxy")]
#[command(
    about = "A simple TCP proxy with systemd socket activation and PROXY protocol v2 support"
)]
struct Args {
    /// Target address to proxy to (HOST:PORT or HOST__PORT for systemd unit instances)
    #[arg(long, env = "TARGET", default_value = "127.0.0.1:8080")]
    target: TargetAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sd_proxy=info".into()),
        )
        .init();

    let args = Args::parse();
    let target_addr = Arc::new(args.target);
    info!(?target_addr.host, ?target_addr.port, "target address");

    let listener = get_listener().await?;
    let local_addr = listener.local_addr()?;
    info!(?local_addr, "listening");

    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())?;

    loop {
        tokio::select! {
            biased;
            _ = sigint.recv() => {
                info!("sigint received");
                break;
            }

            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let target_addr = Arc::clone(&target_addr);
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, addr, target_addr).await {
                                error!(?err, ?addr, "failed to handle client");
                            }
                        });
                    }
                    Err(err) => {
                        error!(?err, "failed to accept");
                    }
                }
            }
        }
    }

    info!("proxy stopped");
    Ok(())
}
