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

/// Optional, peer-supplied description of the content behind a content key.
/// Content keys are one-way hashes, so this is the only way the tracker (and
/// the registry UI on top of it) can know what a key actually is. Last write
/// wins; all fields are optional so pre-metadata clients keep working.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentMeta {
    pub filename: Option<String>,
    pub format: Option<String>,
    pub size: Option<u64>,
}

/// One row of the `GET /contents` listing: a content key that currently has
/// at least one live provider, plus whatever metadata peers supplied for it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentSummary {
    pub content_key: String,
    pub filename: Option<String>,
    pub format: Option<String>,
    pub size: Option<u64>,
    pub providers: usize,
    pub last_seen: u64,
    /// When the first provider for this key announced (the "uploaded" time in
    /// registry listings). Reset if the key fully expires and comes back.
    pub first_seen: u64,
}

pub struct PeerStore {
    data: DashMap<String, Vec<PeerEntry>>,
    meta: DashMap<String, ContentMeta>,
    first_seen: DashMap<String, u64>,
    rate_limits: DashMap<IpAddr, (u32, Instant)>,
    pub ttl_secs: u64,
    pub rate_limit_per_min: u32,
}

impl PeerStore {
    pub fn new(ttl_secs: u64, rate_limit_per_min: u32) -> Arc<Self> {
        Arc::new(Self {
            data: DashMap::new(),
            meta: DashMap::new(),
            first_seen: DashMap::new(),
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

    pub fn announce(
        &self,
        content_key: String,
        node_id: String,
        addr_info: String,
        meta: Option<ContentMeta>,
    ) {
        let entry = PeerEntry {
            node_id: node_id.clone(),
            addr_info,
            last_seen: now_secs(),
        };
        if let Some(meta) = meta {
            self.meta.insert(content_key.clone(), meta);
        }
        self.first_seen
            .entry(content_key.clone())
            .or_insert_with(now_secs);
        let mut peers = self.data.entry(content_key).or_default();
        match peers.iter_mut().find(|p| p.node_id == node_id) {
            Some(existing) => *existing = entry,
            None => peers.push(entry),
        }
    }

    pub fn get_peers(&self, content_key: &str) -> Vec<PeerEntry> {
        let cutoff = now_secs().saturating_sub(self.ttl_secs);
        match self.data.get(content_key) {
            Some(peers) => peers
                .iter()
                .filter(|p| p.last_seen >= cutoff)
                .cloned()
                .collect(),
            None => vec![],
        }
    }

    /// Lists every content key with at least one live provider, most-seeded
    /// first (ties broken by recency). `format` filters on the announced
    /// format (case-insensitive exact match); `query` is a case-insensitive
    /// substring match on the announced filename. Keys announced without
    /// metadata still appear (with null fields) unless a filter excludes them.
    pub fn list_contents(
        &self,
        format: Option<&str>,
        query: Option<&str>,
        limit: usize,
    ) -> Vec<ContentSummary> {
        let cutoff = now_secs().saturating_sub(self.ttl_secs);
        let format = format.map(|f| f.to_ascii_lowercase());
        let query = query.map(|q| q.to_lowercase());

        let mut out: Vec<ContentSummary> = self
            .data
            .iter()
            .filter_map(|entry| {
                let live: Vec<&PeerEntry> = entry
                    .value()
                    .iter()
                    .filter(|p| p.last_seen >= cutoff)
                    .collect();
                if live.is_empty() {
                    return None;
                }
                let meta = self.meta.get(entry.key());
                let (filename, fmt, size) = match meta.as_deref() {
                    Some(m) => (m.filename.clone(), m.format.clone(), m.size),
                    None => (None, None, None),
                };

                if let Some(want) = &format {
                    if fmt.as_deref().map(|f| f.to_ascii_lowercase()).as_deref() != Some(want) {
                        return None;
                    }
                }
                if let Some(q) = &query {
                    match &filename {
                        Some(name) if name.to_lowercase().contains(q.as_str()) => {}
                        _ => return None,
                    }
                }

                let last_seen = live.iter().map(|p| p.last_seen).max().unwrap_or(0);
                Some(ContentSummary {
                    content_key: entry.key().clone(),
                    filename,
                    format: fmt,
                    size,
                    providers: live.len(),
                    last_seen,
                    first_seen: self
                        .first_seen
                        .get(entry.key())
                        .map(|v| *v)
                        .unwrap_or(last_seen),
                })
            })
            .collect();

        out.sort_by(|a, b| {
            b.providers
                .cmp(&a.providers)
                .then(b.last_seen.cmp(&a.last_seen))
        });
        out.truncate(limit);
        out
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
        // Drop metadata and first-seen timestamps for keys that no longer have
        // any providers, so these maps cannot grow without bound once all
        // seeders are gone.
        self.meta.retain(|k, _| self.data.contains_key(k));
        self.first_seen.retain(|k, _| self.data.contains_key(k));
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(filename: &str, format: &str, size: u64) -> ContentMeta {
        ContentMeta {
            filename: Some(filename.to_string()),
            format: Some(format.to_string()),
            size: Some(size),
        }
    }

    #[test]
    fn announce_stores_metadata_and_lists_contents() {
        let store = PeerStore::new(1800, 100);
        store.announce(
            "a".repeat(64),
            "node-1".into(),
            "{}".into(),
            Some(meta("model.safetensors", "safetensors", 42)),
        );
        store.announce("b".repeat(64), "node-2".into(), "{}".into(), None);

        let all = store.list_contents(None, None, 100);
        assert_eq!(all.len(), 2);
        // Metadata-less keys still appear, with null fields.
        let bare = all
            .iter()
            .find(|c| c.content_key == "b".repeat(64))
            .unwrap();
        assert!(bare.filename.is_none() && bare.format.is_none() && bare.size.is_none());
        let described = all
            .iter()
            .find(|c| c.content_key == "a".repeat(64))
            .unwrap();
        assert_eq!(described.filename.as_deref(), Some("model.safetensors"));
        assert_eq!(described.size, Some(42));
        assert_eq!(described.providers, 1);
        assert!(described.first_seen > 0 && described.first_seen <= described.last_seen);
    }

    #[test]
    fn list_contents_filters_by_format_and_query() {
        let store = PeerStore::new(1800, 100);
        store.announce(
            "a".repeat(64),
            "n1".into(),
            "{}".into(),
            Some(meta("Qwen2-0.5B-Q4_K_M.gguf", "gguf", 1)),
        );
        store.announce(
            "b".repeat(64),
            "n2".into(),
            "{}".into(),
            Some(meta("model.safetensors", "safetensors", 2)),
        );

        // Format filter is case-insensitive exact match.
        let ggufs = store.list_contents(Some("GGUF"), None, 100);
        assert_eq!(ggufs.len(), 1);
        assert_eq!(ggufs[0].format.as_deref(), Some("gguf"));

        // Query is a case-insensitive filename substring; keys without a
        // filename never match a query.
        let hits = store.list_contents(None, Some("qwen2"), 100);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].filename.as_deref(), Some("Qwen2-0.5B-Q4_K_M.gguf"));
        assert!(store.list_contents(None, Some("llama"), 100).is_empty());
    }

    #[test]
    fn list_contents_sorts_by_providers_and_respects_limit() {
        let store = PeerStore::new(1800, 100);
        store.announce("a".repeat(64), "n1".into(), "{}".into(), None);
        store.announce("b".repeat(64), "n2".into(), "{}".into(), None);
        store.announce("b".repeat(64), "n3".into(), "{}".into(), None);

        let all = store.list_contents(None, None, 100);
        assert_eq!(all[0].content_key, "b".repeat(64));
        assert_eq!(all[0].providers, 2);

        assert_eq!(store.list_contents(None, None, 1).len(), 1);
    }

    #[test]
    fn last_write_wins_and_eviction_drops_metadata() {
        let store = PeerStore::new(60, 100);
        store.announce(
            "a".repeat(64),
            "n1".into(),
            "{}".into(),
            Some(meta("old.bin", "bin", 1)),
        );
        store.announce(
            "a".repeat(64),
            "n1".into(),
            "{}".into(),
            Some(meta("new.bin", "bin", 2)),
        );
        assert_eq!(store.meta.get(&"a".repeat(64)).unwrap().size, Some(2));

        // Age the only provider far past the ttl: the listing hides the key
        // and eviction drops both the peers and the metadata.
        store
            .data
            .get_mut(&"a".repeat(64))
            .unwrap()
            .iter_mut()
            .for_each(|p| p.last_seen = 1);
        assert!(store.list_contents(None, None, 100).is_empty());
        store.evict_expired();
        assert!(store.data.is_empty());
        assert!(store.meta.is_empty());
        assert!(store.first_seen.is_empty());
    }
}
