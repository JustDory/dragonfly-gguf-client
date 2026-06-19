use crate::tracker::TrackerClient;
use crate::ALPN;
use anyhow::Result;
use dashmap::DashMap;
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, SecretKey};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Wire-protocol handler: serves any file in the shared `files` registry.
// ---------------------------------------------------------------------------

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
            let copied = tokio::io::copy(&mut file, &mut send).await?;
            if copied != file_len {
                return Err(anyhow::anyhow!(
                    "file changed during transfer: expected {} bytes, sent {}",
                    file_len,
                    copied
                ));
            }
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

// ---------------------------------------------------------------------------
// On-disk seed registry: dfget writes manifests, dfdaemon reconciles them.
// ---------------------------------------------------------------------------

/// A single registered seed, persisted as `<content_key>.json` in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedManifest {
    pub content_key: String,
    pub file_path: PathBuf,
    pub tracker_url: String,
    /// Unix seconds after which the seed should be dropped. 0 = never.
    pub expiry_unix: u64,
}

impl SeedManifest {
    fn is_expired(&self) -> bool {
        self.expiry_unix != 0 && now_unix() >= self.expiry_unix
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Default registry directory shared by the writer (dfget) and reader
/// (dfdaemon): `$XDG_DATA_HOME/dragonfly/gguf-seeds` (falling back to
/// `~/.local/share/...`).
pub fn default_registry_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
        })
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("dragonfly").join("gguf-seeds")
}

/// Register a file to be seeded. Writes a manifest atomically and returns
/// immediately; the long-lived seed service (in dfdaemon) picks it up. Safe to
/// call from a short-lived process like dfget.
pub fn register_seed(
    registry_dir: &Path,
    tracker_url: &str,
    content_key: &str,
    file_path: &Path,
    seed_duration: Duration,
) -> Result<()> {
    std::fs::create_dir_all(registry_dir)?;
    let expiry_unix = if seed_duration.is_zero() {
        0
    } else {
        now_unix() + seed_duration.as_secs()
    };
    let manifest = SeedManifest {
        content_key: content_key.to_string(),
        file_path: file_path.to_path_buf(),
        tracker_url: tracker_url.to_string(),
        expiry_unix,
    };
    let dst = registry_dir.join(format!("{content_key}.json"));
    let tmp = registry_dir.join(format!("{content_key}.json.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
    std::fs::rename(&tmp, &dst)?; // atomic replace
    Ok(())
}

fn load_manifests(dir: &Path) -> Vec<(PathBuf, SeedManifest)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<SeedManifest>(&bytes) {
                Ok(m) => out.push((path, m)),
                Err(e) => tracing::warn!("skipping malformed seed manifest {path:?}: {e}"),
            },
            Err(e) => tracing::warn!("cannot read seed manifest {path:?}: {e}"),
        }
    }
    out
}

const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(300);

struct ActiveSeed {
    tracker_url: String,
    last_announce: tokio::time::Instant,
}

/// Long-lived seed service hosted by dfdaemon. Binds one Iroh endpoint that
/// serves every registered file, and reconciles the on-disk registry on a
/// fixed interval: announcing new seeds, re-announcing live ones, and
/// dropping (tracker `leave` + manifest delete) expired ones.
///
/// Returns when `shutdown` resolves.
pub async fn run_seed_service(
    registry_dir: PathBuf,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    std::fs::create_dir_all(&registry_dir)?;

    let files: Arc<DashMap<String, PathBuf>> = Arc::new(DashMap::new());

    let sk = SecretKey::generate();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(sk)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    let node_id = endpoint.id().to_string();
    let addr_info = serde_json::to_string(&endpoint.addr()).unwrap_or_default();
    let router = Router::builder(endpoint)
        .accept(ALPN, Arc::new(GgufProvider { files: files.clone() }))
        .spawn();

    tracing::info!(
        "gguf seed service listening as node {}, registry {:?}",
        &node_id[..8.min(node_id.len())],
        registry_dir
    );

    let mut active: HashMap<String, ActiveSeed> = HashMap::new();
    let mut trackers: HashMap<String, TrackerClient> = HashMap::new();

    let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tick.tick() => {
                reconcile(
                    &registry_dir,
                    &files,
                    &node_id,
                    &addr_info,
                    &mut active,
                    &mut trackers,
                )
                .await;
            }
        }
    }

    // Leave the tracker for every active seed so peers stop being handed a
    // node that is about to disappear.
    for (key, seed) in active.drain() {
        let tracker = trackers
            .entry(seed.tracker_url.clone())
            .or_insert_with(|| TrackerClient::new(seed.tracker_url.clone()));
        let _ = tracker.leave(&key, &node_id).await;
    }
    let _ = router.shutdown().await;
    tracing::info!("gguf seed service stopped");
    Ok(())
}

async fn reconcile(
    registry_dir: &Path,
    files: &Arc<DashMap<String, PathBuf>>,
    node_id: &str,
    addr_info: &str,
    active: &mut HashMap<String, ActiveSeed>,
    trackers: &mut HashMap<String, TrackerClient>,
) {
    let manifests = load_manifests(registry_dir);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (path, manifest) in manifests {
        seen.insert(manifest.content_key.clone());

        // Expired or missing source file: drop it.
        if manifest.is_expired() || !manifest.file_path.exists() {
            if active.remove(&manifest.content_key).is_some() {
                files.remove(&manifest.content_key);
                let tracker = tracker_for(trackers, &manifest.tracker_url);
                let _ = tracker.leave(&manifest.content_key, node_id).await;
            }
            let _ = std::fs::remove_file(&path);
            tracing::debug!(
                "dropped expired seed {}",
                &manifest.content_key[..8.min(manifest.content_key.len())]
            );
            continue;
        }

        files.insert(manifest.content_key.clone(), manifest.file_path.clone());

        match active.get_mut(&manifest.content_key) {
            None => {
                let tracker = tracker_for(trackers, &manifest.tracker_url);
                match tracker.announce(&manifest.content_key, node_id, addr_info).await {
                    Ok(()) => {
                        tracing::info!(
                            "seeding {} ({:?})",
                            &manifest.content_key[..8.min(manifest.content_key.len())],
                            manifest.file_path
                        );
                        active.insert(
                            manifest.content_key.clone(),
                            ActiveSeed {
                                tracker_url: manifest.tracker_url.clone(),
                                last_announce: tokio::time::Instant::now(),
                            },
                        );
                    }
                    Err(e) => tracing::warn!(
                        "announce failed for {}: {e}",
                        &manifest.content_key[..8.min(manifest.content_key.len())]
                    ),
                }
            }
            Some(seed) => {
                if seed.last_announce.elapsed() >= REANNOUNCE_INTERVAL {
                    let tracker = tracker_for(trackers, &manifest.tracker_url);
                    if let Err(e) =
                        tracker.announce(&manifest.content_key, node_id, addr_info).await
                    {
                        tracing::warn!(
                            "re-announce failed for {}: {e}",
                            &manifest.content_key[..8.min(manifest.content_key.len())]
                        );
                    } else {
                        seed.last_announce = tokio::time::Instant::now();
                    }
                }
            }
        }
    }

    // Manifests removed out from under us: stop serving/announcing them.
    let stale: Vec<String> = active
        .keys()
        .filter(|k| !seen.contains(*k))
        .cloned()
        .collect();
    for key in stale {
        if let Some(seed) = active.remove(&key) {
            files.remove(&key);
            let tracker = tracker_for(trackers, &seed.tracker_url);
            let _ = tracker.leave(&key, node_id).await;
        }
    }
}

fn tracker_for<'a>(
    trackers: &'a mut HashMap<String, TrackerClient>,
    url: &str,
) -> &'a TrackerClient {
    trackers
        .entry(url.to_string())
        .or_insert_with(|| TrackerClient::new(url.to_string()))
}

// ---------------------------------------------------------------------------
// Test helpers + tests
// ---------------------------------------------------------------------------

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

    let addr_info = serde_json::to_string(&endpoint.addr()).unwrap_or_default();
    let node_id = endpoint.id().to_string();

    let router = iroh::protocol::Router::builder(endpoint)
        .accept(crate::ALPN, Arc::new(GgufProvider { files }))
        .spawn();

    Ok((node_id, addr_info, router))
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
        assert_eq!(
            downloaded.as_slice(),
            content,
            "downloaded content does not match source"
        );
    }

    #[test]
    fn test_register_seed_writes_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let key = "b".repeat(64);
        let file = dir.path().join("model.gguf");
        std::fs::write(&file, b"x").unwrap();

        register_seed(
            dir.path(),
            "http://tracker.example:8080",
            &key,
            &file,
            Duration::from_secs(3600),
        )
        .unwrap();

        let loaded = load_manifests(dir.path());
        assert_eq!(loaded.len(), 1);
        let (_, m) = &loaded[0];
        assert_eq!(m.content_key, key);
        assert_eq!(m.file_path, file);
        assert_eq!(m.tracker_url, "http://tracker.example:8080");
        assert!(m.expiry_unix > now_unix(), "expiry should be in the future");
        assert!(!m.is_expired());
    }

    #[test]
    fn test_expired_manifest_detected() {
        let m = SeedManifest {
            content_key: "c".repeat(64),
            file_path: PathBuf::from("/tmp/nope.gguf"),
            tracker_url: "http://t".to_string(),
            expiry_unix: 1, // 1970
        };
        assert!(m.is_expired());

        let never = SeedManifest {
            expiry_unix: 0,
            ..m.clone()
        };
        assert!(!never.is_expired(), "expiry_unix 0 means never expires");
    }
}
