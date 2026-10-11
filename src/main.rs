mod endpoints;
mod service;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};

const LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8000);

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind(LISTEN).await?;
    let mut terminate = signal(SignalKind::terminate())?;
    // PID 1 ignores SIGTERM without a handler, and a graceful shutdown would wait on
    // watches that never end.
    tokio::select! {
        served = endpoints::serve(listener, Default::default()) => served,
        _ = terminate.recv() => Ok(()),
    }
}
