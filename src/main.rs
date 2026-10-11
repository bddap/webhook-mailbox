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
    endpoints::serve(listener, Default::default(), async move {
        terminate.recv().await;
    })
    .await
}
