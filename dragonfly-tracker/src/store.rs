use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerEntry {
    pub node_id: String,
    pub addr_info: String,
    pub last_seen: u64,
}

pub struct PeerStore {
    data: DashMap<String, Vec<PeerEntry>>,
    rate_limits: DashMap<IpAddr, (u32, Instant)>,
    pub ttl_secs: u64,
    pub rate_limit_per_min: u32,
}

impl PeerStore {
    pub fn new(ttl_secs: u64, rate_limit_per_min: u32) -> Arc<Self> {
        Arc::new(Self {
            data: DashMap::new(),
            rate_limits: DashMap::new(),
            ttl_secs,
            rate_limit_per_min,
        })
    }

    pub fn check_rate_limit(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut entry = self.rate_limits.entry(ip).or_insert((0, now));
        let (count, window_start) = entry.value_mut();
        if now.duration_since(*window_start).as_secs() >= 60 {
            *count = 0;
            *window_start = now;
        }
        if *count >= self.rate_limit_per_min {
            return false;
        }
        *count += 1;
        true
    }

    pub fn announce(&self, content_key: String, node_id: String, addr_info: String) {
        let entry = PeerEntry {
            node_id: node_id.clone(),
            addr_info,
            last_seen: now_secs(),
        };
        let mut peers = self.data.entry(content_key).or_default();
        match peers.iter_mut().find(|p| p.node_id == node_id) {
            Some(existing) => *existing = entry,
            None => peers.push(entry),
        }
    }

    pub fn get_peers(&self, content_key: &str) -> Vec<PeerEntry> {
        let cutoff = now_secs().saturating_sub(self.ttl_secs);
        match self.data.get(content_key) {
            Some(peers) => peers.iter().filter(|p| p.last_seen >= cutoff).cloned().collect(),
            None => vec![],
        }
    }

    pub fn remove_peer(&self, content_key: &str, node_id: &str) {
        if let Some(mut peers) = self.data.get_mut(content_key) {
            peers.retain(|p| p.node_id != node_id);
        }
    }

    pub fn evict_expired(&self) {
        let cutoff = now_secs().saturating_sub(self.ttl_secs);
        for mut entry in self.data.iter_mut() {
            entry.value_mut().retain(|p| p.last_seen >= cutoff);
        }
        self.data.retain(|_, v| !v.is_empty());
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
