use crate::node::IrohNode;
use crate::tracker::PeerEntry;
use crate::ALPN;
use anyhow::Result;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;

/// Maximum number of peers to dial concurrently while racing for the first one
/// that is reachable and has the content. Keeps a large swarm from opening an
/// unbounded number of connections at once; as each dial fails the next queued
/// peer is started in its place.
const MAX_CONCURRENT_DIALS: usize = 8;

/// Upper bound on how long a single peer's connect + handshake may take before
/// we give up on it. This is deliberately short and independent of the overall
/// download `timeout` (which is sized for the full body transfer): dialing is
/// raced across peers, so an offline or packet-dropping peer must fail fast
/// instead of stalling the whole download.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A peer that has been connected to and has confirmed it holds the content.
/// The handshake (status byte + length prefix) is already consumed, so `recv`
/// is positioned at the first body byte. `conn` is held to keep the QUIC
/// connection (and therefore `recv`) alive for the duration of the transfer.
struct ReadyPeer {
    node_id: String,
    file_len: u64,
    // Held purely to keep the QUIC connection (and therefore `recv`) alive for
    // the duration of the body transfer; never read directly.
    #[allow(dead_code)]
    conn: iroh::endpoint::Connection,
    recv: iroh::endpoint::RecvStream,
}

pub async fn download_from_peers(
    node: &IrohNode,
    providers: Vec<PeerEntry>,
    content_key: &str,
    output: &Path,
    timeout: Duration,
) -> Result<()> {
    if providers.is_empty() {
        return Err(anyhow::anyhow!("no providers"));
    }

    // Phase 1 — race the connect + handshake across peers concurrently. The
    // first peer that answers "I have it" and is ready to stream wins; every
    // other in-flight dial is then aborted. Because the dials run in parallel,
    // the time to find a good peer is bounded by a single CONNECT_TIMEOUT, not
    // the sum over every dead peer ahead of it in the list.
    let connect_timeout = timeout.min(CONNECT_TIMEOUT);
    let endpoint = node.endpoint.clone();
    let key = content_key.to_string();

    let mut queue = providers.into_iter();
    let mut dials: JoinSet<Result<ReadyPeer>> = JoinSet::new();

    for peer in queue.by_ref().take(MAX_CONCURRENT_DIALS) {
        spawn_dial(&mut dials, &endpoint, &key, peer, connect_timeout);
    }

    let mut last_err = anyhow::anyhow!("no providers reachable");
    let mut winner: Option<ReadyPeer> = None;

    while let Some(joined) = dials.join_next().await {
        match joined {
            Ok(Ok(ready)) => {
                winner = Some(ready);
                break;
            }
            Ok(Err(e)) => {
                tracing::warn!("dial failed: {e}");
                last_err = e;
                // Backfill with the next queued peer so we keep up to
                // MAX_CONCURRENT_DIALS attempts in flight.
                if let Some(peer) = queue.next() {
                    spawn_dial(&mut dials, &endpoint, &key, peer, connect_timeout);
                }
            }
            Err(join_err) => {
                last_err = anyhow::anyhow!("dial task failed: {join_err}");
            }
        }
    }

    // Abort any still-running dials; we only need one peer to stream from.
    dials.abort_all();

    let ready = match winner {
        Some(r) => r,
        None => return Err(last_err),
    };

    // Phase 2 — pull the body from the chosen peer, using the full `timeout`
    // for the (potentially large) transfer.
    download_body(ready, output, timeout).await
}

/// Spawn a single peer dial onto the JoinSet, bounded by `connect_timeout`.
fn spawn_dial(
    dials: &mut JoinSet<Result<ReadyPeer>>,
    endpoint: &iroh::Endpoint,
    content_key: &str,
    peer: PeerEntry,
    connect_timeout: Duration,
) {
    let endpoint = endpoint.clone();
    let content_key = content_key.to_string();
    dials.spawn(async move {
        match tokio::time::timeout(connect_timeout, dial_peer(&endpoint, &peer, &content_key)).await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "peer {} connect timed out",
                &peer.node_id[..8.min(peer.node_id.len())]
            )),
        }
    });
}

/// Connect to a peer, send the content-key request, and read the handshake
/// (status + length). Returns a `ReadyPeer` positioned at the first body byte,
/// or an error if the peer is unreachable or does not have the content.
async fn dial_peer(
    endpoint: &iroh::Endpoint,
    peer: &PeerEntry,
    content_key: &str,
) -> Result<ReadyPeer> {
    let peer_id: iroh::EndpointId = peer.node_id.parse()?;
    // Use the full EndpointAddr (with relay + direct addrs) when available so we
    // can connect without a relay lookup — critical for local/test scenarios.
    let conn = if let Ok(ep_addr) = serde_json::from_str::<iroh::EndpointAddr>(&peer.addr_info) {
        endpoint.connect(ep_addr, ALPN).await?
    } else {
        endpoint.connect(peer_id, ALPN).await?
    };
    let (mut send, mut recv) = conn.open_bi().await?;

    let key_bytes = content_key.as_bytes();
    send.write_all(&(key_bytes.len() as u32).to_le_bytes())
        .await?;
    send.write_all(key_bytes).await?;
    send.finish()?;

    let mut status = [0u8; 1];
    recv.read_exact(&mut status).await?;
    if status[0] == 0 {
        return Err(anyhow::anyhow!("peer does not have this content"));
    }

    let mut len_buf = [0u8; 8];
    recv.read_exact(&mut len_buf).await?;
    let file_len = u64::from_le_bytes(len_buf);

    tracing::debug!(
        "peer {} ready: {:.1} MiB",
        &peer.node_id[..8.min(peer.node_id.len())],
        file_len as f64 / 1_048_576.0
    );

    Ok(ReadyPeer {
        node_id: peer.node_id.clone(),
        file_len,
        conn,
        recv,
    })
}

/// Stream the body from a chosen peer to `output`, bounded by `timeout`. Any
/// short read (or a transfer that runs over the timeout) deletes the partial
/// file rather than leaving a truncated model on disk — SHA verification may be
/// skipped when no digest is advertised, so we must never hand back a corrupt
/// file.
async fn download_body(mut ready: ReadyPeer, output: &Path, timeout: Duration) -> Result<()> {
    tracing::info!(
        "downloading {:.1} MiB via P2P from {}",
        ready.file_len as f64 / 1_048_576.0,
        &ready.node_id[..8.min(ready.node_id.len())]
    );

    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(output)
        .await?;

    let copied =
        match tokio::time::timeout(timeout, tokio::io::copy(&mut ready.recv, &mut file)).await {
            Ok(result) => result?,
            Err(_) => {
                drop(file);
                let _ = tokio::fs::remove_file(output).await;
                return Err(anyhow::anyhow!("P2P body transfer timed out"));
            }
        };
    file.flush().await?;

    if copied != ready.file_len {
        drop(file);
        let _ = tokio::fs::remove_file(output).await;
        return Err(anyhow::anyhow!(
            "P2P transfer size mismatch: expected {} bytes, got {copied}",
            ready.file_len
        ));
    }

    tracing::info!("P2P download complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::IrohNode;
    use crate::seeder::spawn_test_seeder;
    use std::time::Instant;

    /// A dead peer listed *before* a live one must not stall the download: the
    /// concurrent dial races both, so the live peer wins long before the dead
    /// peer's connect timeout elapses. With the old sequential loop this would
    /// block for the full CONNECT_TIMEOUT on the dead peer first.
    #[tokio::test]
    async fn test_dead_peer_first_does_not_stall() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("model.gguf");
        let dst = dir.path().join("downloaded.gguf");
        let content = b"GGUF fake model content for dead-peer failover test";
        tokio::fs::write(&src, content).await.unwrap();

        let key = "a".repeat(64);
        let (node_id, addr_info, router) =
            spawn_test_seeder(key.clone(), src.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let dl_node = IrohNode::new(None).await.unwrap();

        // A syntactically valid but unreachable peer listed first, then the
        // real seeder. We mint a real endpoint id from a throwaway node and
        // immediately close it, so the id parses but never answers.
        let dead_node = IrohNode::new(None).await.unwrap();
        let dead_id = dead_node.endpoint.id().to_string();
        dead_node.close().await;
        let dead = PeerEntry {
            node_id: dead_id,
            addr_info: "{}".to_string(),
            last_seen: 0,
        };
        let good = PeerEntry {
            node_id,
            addr_info,
            last_seen: 0,
        };

        let start = Instant::now();
        let result = download_from_peers(
            &dl_node,
            vec![dead, good],
            &key,
            &dst,
            Duration::from_secs(15),
        )
        .await;
        let elapsed = start.elapsed();

        dl_node.close().await;
        let _ = router.shutdown().await;

        result.expect("should download from the live peer despite a dead peer first");
        assert!(
            elapsed < CONNECT_TIMEOUT,
            "concurrent dial should beat the dead peer's connect timeout, took {elapsed:?}"
        );
        let downloaded = tokio::fs::read(&dst).await.unwrap();
        assert_eq!(downloaded.as_slice(), content);
    }
}
