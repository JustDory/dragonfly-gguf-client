mod downloader;
mod node;
mod seeder;
mod tracker;

pub use seeder::{default_registry_dir, register_seed, run_seed_service, SeedManifest};
pub use tracker::TrackerClient;

use anyhow::Result;
use sha2::Digest;
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_TRACKER_URL: &str = "https://tracker.dragonfly-gguf.dev";

pub(crate) const ALPN: &[u8] = b"/dragonfly-gguf/1";

pub fn content_key(gguf_url: &str, revision: &str) -> String {
    let hf_url = gguf_url
        .strip_prefix("gguf://")
        .map(|rest| format!("hf://{rest}"))
        .unwrap_or_else(|| gguf_url.to_string());
    let canonical = hf_url.to_lowercase();
    let input = format!("{canonical}:{revision}");
    hex::encode(sha2::Sha256::digest(input.as_bytes()))
}

pub async fn try_p2p_download(
    tracker_url: &str,
    content_key: &str,
    output: &Path,
    keypair_path: Option<&Path>,
    timeout: Duration,
) -> Result<()> {
    let tracker = TrackerClient::new(tracker_url.to_string());
    let peers = tracker.get_peers(content_key).await.map_err(|e| {
        anyhow::anyhow!("tracker query failed: {e}")
    })?;

    if peers.is_empty() {
        return Err(anyhow::anyhow!("no P2P providers"));
    }

    tracing::info!(
        "found {} P2P provider(s) for {}...",
        peers.len(),
        &content_key[..8]
    );

    let node = node::IrohNode::new(keypair_path).await?;
    let result = downloader::download_from_peers(&node, peers, content_key, output, timeout).await;
    node.close().await;
    result
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_key_stability() {
        let k1 = content_key("gguf://owner/repo/model.gguf", "main");
        let k2 = content_key("gguf://owner/repo/model.gguf", "main");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64);
    }

    #[test]
    fn test_content_key_normalization() {
        let k1 = content_key("gguf://Owner/Repo/Model.gguf", "main");
        let k2 = content_key("gguf://owner/repo/model.gguf", "main");
        assert_eq!(k1, k2, "content_key must be case-insensitive");
    }

    #[test]
    fn test_content_key_revision_matters() {
        let k1 = content_key("gguf://owner/repo/model.gguf", "main");
        let k2 = content_key("gguf://owner/repo/model.gguf", "v1.0");
        assert_ne!(k1, k2);
    }
}
