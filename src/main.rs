use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use eyre::{Context, ContextCompat, Result, bail, eyre};
use listenfd::ListenFd;
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

pub struct TargetConnection {
    pub stream: TfoStream,
    pub target: SocketAddr,
    pub normalized_target: SocketAddr,
}

async fn establish_proxy_connection(
    addr: SocketAddr,
    normalized_client: SocketAddr,
    normalized_target: SocketAddr,
    use_v2_header: bool,
) -> eyre::Result<TfoStream> {
    let mut stream = TfoStream::connect(addr).await?;
    stream.set_nodelay(true).wrap_err("failed to set nodelay")?;

    trace!(?addr, ?normalized_client, "initial connection set up");

    if use_v2_header {
        let header = ppp::v2::Builder::with_addresses(
            ppp::v2::Version::Two | ppp::v2::Command::Proxy,
            ppp::v2::Protocol::Stream,
            (normalized_client, normalized_target),
        )
        .build()
        .wrap_err("failed to build PROXY protocol v2 header")?;

        stream
            .write_all(&header)
            .await
            .wrap_err("failed to write PROXY header")?;
    } else {
        let addresses = match ppp::v1::Addresses::from((normalized_client, normalized_target)) {
            ppp::v1::Addresses::Unknown => bail!(
                "source & destination ip version mismatch: {} != {}",
                normalized_client,
                normalized_target,
            ),
            v => v,
        };

        stream
            .write_all(format!("{}", addresses).as_bytes())
            .await
            .wrap_err("failed to write PROXY header")?;
    };

    trace!(
        ?addr,
        ?normalized_client,
        use_v2_header,
        "wrote proxy header"
    );

    Ok(stream)
}

async fn connect_to_target(
    filtered_targets: &[SocketAddr],
    normalized_client: SocketAddr,
    use_v2_header: bool,
) -> Result<TargetConnection> {
    let mut result = None;
    let mut last_error = None;

    for addr in filtered_targets {
        let normalized_target = normalize_addr(*addr);

        match establish_proxy_connection(*addr, normalized_client, normalized_target, use_v2_header)
            .await
        {
            Ok(stream) => {
                debug!(?addr, "connection established");
                result = Some(TargetConnection {
                    stream,
                    target: *addr,
                    normalized_target,
                });
                break;
            }
            Err(err) => {
                debug!(?addr, ?err, "failed to establish connection, trying next");
                last_error = Some(err);
            }
        }
    }

    result.ok_or_else(|| {
        last_error
            .map(|err| eyre!("failed to connect to any target address: {:?}", err))
            .unwrap_or_else(|| eyre!("no target addresses to connect to"))
    })
}

async fn handle_client(
    client_stream: TfoStream,
    client_addr: SocketAddr,
    target_addr: Arc<TargetAddr>,
    strict_ip_version_mapping: bool,
    use_v2_header: bool,
) -> Result<()> {
    let normalized_client = normalize_addr(client_addr);
    debug!(?client_addr, ?normalized_client, "new inbound connection");

    client_stream.set_nodelay(true)?;

    let filtered_targets = target_addr
        .resolve(strict_ip_version_mapping.then_some(normalized_client))
        .await
        .wrap_err("failed to resolve target address")?;

    debug!(
        ?filtered_targets,
        ?target_addr.host,
        ?target_addr.port,
        "resolved target addresses"
    );

    let TargetConnection {
        stream: target_stream,
        target,
        normalized_target,
    } = connect_to_target(&filtered_targets, normalized_client, use_v2_header).await?;

    debug!(?target, ?client_addr, "connection established");

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

    /// Whether to match client ip version with target ip version. Setting this option requires dual-stack support
    /// on backend services as well, and implies --target is DNS which will resolve both A and AAAA records.
    #[arg(long, env = "STRICT_IP_VERSION_MAPPING", default_value_t = false)]
    strict_ip_version_mapping: bool,

    /// Whether to use v2 PROXY header or not.
    #[arg(long, env = "USE_V2_HEADER", default_value_t = true)]
    use_v2_header: bool,
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
            _ = sigint.recv() => {
                info!("sigint received");
                break;
            }

            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let target_addr = Arc::clone(&target_addr);
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, addr, target_addr, args.strict_ip_version_mapping, args.use_v2_header).await {
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
