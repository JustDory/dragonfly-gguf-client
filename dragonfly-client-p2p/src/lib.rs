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

/// Derive the stable P2P content key for a `gguf://` (or `hf://`) URL.
///
/// The key binds the source host, the canonical owner/repo/path and the
/// revision, so two peers only ever exchange content that is byte-for-byte the
/// same source object:
///   * `base_url` is included so the same path served by two different
///     Hugging Face-compatible mirrors never collides on one key.
///   * the path is **not** lowercased — Hugging Face files are git blobs and are
///     therefore case-sensitive, so `Model.gguf` and `model.gguf` must map to
///     distinct keys (otherwise a peer could serve the wrong file).
pub fn content_key(gguf_url: &str, revision: &str, base_url: Option<&str>) -> String {
    let hf_url = gguf_url
        .strip_prefix("gguf://")
        .map(|rest| format!("hf://{rest}"))
        .unwrap_or_else(|| gguf_url.to_string());
    let input = match base_url {
        Some(b) if !b.is_empty() => format!("{hf_url}:{revision}:{b}"),
        _ => format!("{hf_url}:{revision}"),
    };
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
        let k1 = content_key("gguf://owner/repo/model.gguf", "main", None);
        let k2 = content_key("gguf://owner/repo/model.gguf", "main", None);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64);
    }

    #[test]
    fn test_content_key_is_case_sensitive() {
        // Hugging Face files are case-sensitive git blobs, so distinct casing must
        // produce distinct keys — a peer must never serve the wrong file.
        let k1 = content_key("gguf://Owner/Repo/Model.gguf", "main", None);
        let k2 = content_key("gguf://owner/repo/model.gguf", "main", None);
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_content_key_revision_matters() {
        let k1 = content_key("gguf://owner/repo/model.gguf", "main", None);
        let k2 = content_key("gguf://owner/repo/model.gguf", "v1.0", None);
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_content_key_base_url_matters() {
        // The same path on two different mirrors must not collide.
        let k1 = content_key("gguf://owner/repo/model.gguf", "main", Some("https://hf.co"));
        let k2 = content_key(
            "gguf://owner/repo/model.gguf",
            "main",
            Some("https://mirror.example"),
        );
        assert_ne!(k1, k2);

        // None and "" are the same default.
        let k3 = content_key("gguf://owner/repo/model.gguf", "main", None);
        let k4 = content_key("gguf://owner/repo/model.gguf", "main", Some(""));
        assert_eq!(k3, k4);
    }

    #[test]
    fn test_content_key_no_spurious_pipe() {
        // Regression: earlier impl used format!("{base}|{hf_url}:{revision}") which
        // produced a leading "|" when base_url is None, giving a wrong hash.
        // Verify the input string is "hf://...:main", not "|hf://...:main".
        let k_no_base = content_key("hf://owner/repo/model.gguf", "main", None);
        let k_pipe = hex::encode(sha2::Sha256::digest(b"|hf://owner/repo/model.gguf:main"));
        assert_ne!(k_no_base, k_pipe, "content_key must not include a spurious leading pipe");
        let expected = hex::encode(sha2::Sha256::digest(b"hf://owner/repo/model.gguf:main"));
        assert_eq!(k_no_base, expected);
    }
}
