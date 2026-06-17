use crate::tracker::TrackerClient;
use crate::ALPN;
use anyhow::Result;
use dashmap::DashMap;
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, SecretKey};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
struct GgufProvider {
    files: Arc<DashMap<String, PathBuf>>,
}

impl ProtocolHandler for GgufProvider {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let files = self.files.clone();
        accept_inner(conn, files)
            .await
            .map_err(|e| AcceptError::from_err(AnyErrorWrapper(e)))
    }
}

async fn accept_inner(conn: Connection, files: Arc<DashMap<String, PathBuf>>) -> Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await?;

    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let key_len = u32::from_le_bytes(len_buf) as usize;
    if key_len > 256 {
        return Err(anyhow::anyhow!("key too long"));
    }

    let mut key_bytes = vec![0u8; key_len];
    recv.read_exact(&mut key_bytes).await?;
    let content_key = String::from_utf8(key_bytes)?;

    match files.get(&content_key) {
        Some(path) => {
            let mut file = tokio::fs::File::open(path.value()).await?;
            let metadata = file.metadata().await?;
            let file_len = metadata.len();

            send.write_all(&[1u8]).await?;
            send.write_all(&file_len.to_le_bytes()).await?;
            tokio::io::copy(&mut file, &mut send).await?;
            send.finish()?;
            tracing::debug!(
                "served {} bytes for key {}",
                file_len,
                &content_key[..8.min(content_key.len())]
            );
        }
        None => {
            send.write_all(&[0u8]).await?;
            send.finish()?;
        }
    }

    // Wait for the client to close before returning; dropping `conn` without
    // waiting sends a QUIC CONNECTION_CLOSE that can arrive before the client
    // has finished reading the last bytes we sent.
    let _ = conn.closed().await;
    Ok(())
}

/// Newtype wrapper so anyhow::Error can be passed to AcceptError::from_err.
struct AnyErrorWrapper(anyhow::Error);

impl std::fmt::Debug for AnyErrorWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::fmt::Display for AnyErrorWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AnyErrorWrapper {}

#[cfg(test)]
pub(crate) async fn spawn_test_seeder(
    content_key: String,
    file_path: std::path::PathBuf,
) -> anyhow::Result<(String, String, iroh::protocol::Router)> {
    let files: Arc<DashMap<String, PathBuf>> = Arc::new(DashMap::new());
    files.insert(content_key, file_path);

    let sk = iroh::SecretKey::generate();
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(sk)
        .alpns(vec![crate::ALPN.to_vec()])
        .bind()
        .await?;

    let node_id = endpoint.id().to_string();
    let addr_info = serde_json::to_string(&endpoint.addr()).unwrap_or_default();

    let router = iroh::protocol::Router::builder(endpoint)
        .accept(crate::ALPN, Arc::new(GgufProvider { files }))
        .spawn();

    Ok((node_id, addr_info, router))
}

pub async fn run_seeder(
    tracker_url: String,
    content_key: String,
    file_path: PathBuf,
    seed_duration: Duration,
) -> Result<()> {
    let files: Arc<DashMap<String, PathBuf>> = Arc::new(DashMap::new());
    files.insert(content_key.clone(), file_path);

    let sk = SecretKey::generate();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(sk)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;

    let node_id = endpoint.id().to_string();
    let addr_info = serde_json::to_string(&endpoint.addr()).unwrap_or_default();

    let router = Router::builder(endpoint)
        .accept(ALPN, Arc::new(GgufProvider { files }))
        .spawn();

    let tracker = TrackerClient::new(tracker_url.clone());
    if let Err(e) = tracker.announce(&content_key, &node_id, &addr_info).await {
        tracing::warn!("initial announce failed: {e}");
    } else {
        tracing::info!(
            "seeding {}... as node {}...",
            &content_key[..8],
            &node_id[..8.min(node_id.len())]
        );
    }

    let end = tokio::time::Instant::now() + seed_duration;
    let mut reannounce = tokio::time::interval(Duration::from_secs(300));
    reannounce.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = reannounce.tick() => {
                if let Err(e) = tracker.announce(&content_key, &node_id, &addr_info).await {
                    tracing::warn!("re-announce failed: {e}");
                }
            }
            _ = tokio::time::sleep_until(end) => {
                break;
            }
        }
    }

    let _ = tracker.leave(&content_key, &node_id).await;
    let _ = router.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader;
    use crate::node::IrohNode;
    use crate::tracker::PeerEntry;
    use std::time::Duration;

    #[tokio::test]
    async fn test_seeder_downloader_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("model.gguf");
        let dst = dir.path().join("downloaded.gguf");
        let content = b"GGUF fake model content for P2P loopback test";
        tokio::fs::write(&src, content).await.unwrap();

        let key = "a".repeat(64);

        let (node_id, addr_info, router) =
            spawn_test_seeder(key.clone(), src.clone()).await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let dl_node = IrohNode::new(None).await.unwrap();

        let peer = PeerEntry {
            node_id,
            addr_info,
            last_seen: 0,
        };

        let result = downloader::download_from_peers(
            &dl_node,
            vec![peer],
            &key,
            &dst,
            Duration::from_secs(15),
        )
        .await;

        dl_node.close().await;
        let _ = router.shutdown().await;

        result.expect("P2P loopback download failed");

        let downloaded = tokio::fs::read(&dst).await.unwrap();
        assert_eq!(downloaded.as_slice(), content, "downloaded content does not match source");
    }
}
