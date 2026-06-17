mod routes;
mod store;

use clap::Parser;
use std::net::SocketAddr;

#[derive(Debug, Parser)]
#[command(
    name = "dragonfly-tracker",
    about = "Peer discovery tracker for Dragonfly GGUF P2P distribution"
)]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8080", env = "TRACKER_BIND")]
    bind: SocketAddr,

    #[arg(long, default_value_t = 1800, env = "TRACKER_TTL",
          help = "Peer entry TTL in seconds")]
    ttl: u64,

    #[arg(long, default_value_t = 10, env = "TRACKER_RATE_LIMIT",
          help = "Max announce requests per IP per minute")]
    rate_limit: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let store = store::PeerStore::new(args.ttl, args.rate_limit);

    let evict_store = store.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            evict_store.evict_expired();
        }
    });

    tracing::info!("dragonfly-tracker listening on {}", args.bind);
    warp::serve(routes::routes(store)).run(args.bind).await;
    Ok(())
}
