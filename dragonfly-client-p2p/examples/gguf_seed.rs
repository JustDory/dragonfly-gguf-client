//! PoC seeder: registers a file for seeding (the exact code path dfget runs
//! after a successful download) and then runs the production seed service.
//!
//! Args: <tracker_url> <gguf_url> <revision> <file> <registry_dir>
use dragonfly_client_p2p as p2p;
use std::path::PathBuf;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (tracker, url, revision, file, registry) =
        (&a[1], &a[2], &a[3], PathBuf::from(&a[4]), PathBuf::from(&a[5]));

    let key = p2p::content_key(url, revision, None);
    p2p::register_seed(&registry, tracker, &key, &file, Duration::from_secs(600))?;
    eprintln!("[seeder] serving content_key={key}");
    eprintln!("[seeder]   (derived purely from the public URL {url} @ {revision})");

    // Never returns; killed by the test harness.
    p2p::run_seed_service(registry, std::future::pending::<()>()).await?;
    Ok(())
}
