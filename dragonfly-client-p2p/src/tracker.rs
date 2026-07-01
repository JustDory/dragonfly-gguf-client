use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerEntry {
    pub node_id: String,
    pub addr_info: String,
    pub last_seen: u64,
}

#[derive(Debug, Deserialize)]
struct PeersResponse {
    providers: Vec<PeerEntry>,
}

/// Optional content metadata sent with an announce so the tracker (and any
/// registry UI on top of it) can describe what a content key actually is —
/// keys are one-way hashes, so this is the only self-describing channel.
/// All fields are optional; older trackers ignore unknown announce fields,
/// so sending metadata is always safe.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileMeta {
    pub filename: Option<String>,
    pub format: Option<String>,
    pub size: Option<u64>,
}

impl FileMeta {
    /// Derives metadata from a local file: name and lowercased extension from
    /// the path, size from the filesystem (best-effort).
    pub fn from_path(path: &std::path::Path) -> Self {
        Self {
            filename: path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_string()),
            format: path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase()),
            size: std::fs::metadata(path).ok().map(|m| m.len()),
        }
    }
}

pub struct TrackerClient {
    url: String,
    client: reqwest::Client,
}

impl TrackerClient {
    pub fn new(url: String) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest client build"),
        }
    }

    pub async fn get_peers(&self, content_key: &str) -> Result<Vec<PeerEntry>> {
        let url = format!("{}/peers?content_key={content_key}", self.url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("tracker unreachable: {e}"))?;

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("tracker returned {}", resp.status()));
        }

        let body: PeersResponse = resp.json().await?;
        Ok(body.providers)
    }

    pub async fn announce(&self, content_key: &str, node_id: &str, addr_info: &str) -> Result<()> {
        self.announce_with_meta(content_key, node_id, addr_info, None)
            .await
    }

    /// Announce with optional content metadata (filename, format, size) so the
    /// tracker can list what this peer is actually seeding.
    pub async fn announce_with_meta(
        &self,
        content_key: &str,
        node_id: &str,
        addr_info: &str,
        meta: Option<&FileMeta>,
    ) -> Result<()> {
        let url = format!("{}/announce", self.url);
        let mut body = serde_json::json!({
            "content_key": content_key,
            "node_id": node_id,
            "addr_info": addr_info,
        });
        if let Some(meta) = meta {
            let map = body.as_object_mut().expect("body is a JSON object");
            if let Some(filename) = &meta.filename {
                map.insert("filename".into(), serde_json::json!(filename));
            }
            if let Some(format) = &meta.format {
                map.insert("format".into(), serde_json::json!(format));
            }
            if let Some(size) = meta.size {
                map.insert("size".into(), serde_json::json!(size));
            }
        }
        let resp = self.client.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("announce failed: {}", resp.status()));
        }
        Ok(())
    }

    pub async fn leave(&self, content_key: &str, node_id: &str) -> Result<()> {
        let url = format!("{}/leave", self.url);
        let body = serde_json::json!({
            "content_key": content_key,
            "node_id": node_id,
        });
        let _ = self.client.delete(&url).json(&body).send().await;
        Ok(())
    }
}
