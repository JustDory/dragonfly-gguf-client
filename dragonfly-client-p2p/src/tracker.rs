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
        let url = format!("{}/announce", self.url);
        let body = serde_json::json!({
            "content_key": content_key,
            "node_id": node_id,
            "addr_info": addr_info,
        });
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
