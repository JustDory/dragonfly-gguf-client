use crate::node::IrohNode;
use crate::tracker::PeerEntry;
use crate::ALPN;
use anyhow::Result;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub async fn download_from_peers(
    node: &IrohNode,
    providers: Vec<PeerEntry>,
    content_key: &str,
    output: &Path,
    timeout: Duration,
) -> Result<()> {
    let mut last_err = anyhow::anyhow!("no providers");
    for peer in providers {
        tracing::debug!(
            "trying provider {}",
            &peer.node_id[..8.min(peer.node_id.len())]
        );
        match tokio::time::timeout(
            timeout,
            try_download_from_peer(&node.endpoint, &peer, content_key, output),
        )
        .await
        {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => {
                tracing::warn!(
                    "provider {} failed: {e}",
                    &peer.node_id[..8.min(peer.node_id.len())]
                );
                last_err = e;
            }
            Err(_) => {
                tracing::warn!(
                    "provider {} timed out",
                    &peer.node_id[..8.min(peer.node_id.len())]
                );
                last_err = anyhow::anyhow!("timeout");
            }
        }
    }
    Err(last_err)
}

async fn try_download_from_peer(
    endpoint: &iroh::Endpoint,
    peer: &PeerEntry,
    content_key: &str,
    output: &Path,
) -> Result<()> {
    let peer_id: iroh::EndpointId = peer.node_id.parse()?;
    // Use full EndpointAddr (with relay + direct addrs) when available so we
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
    tracing::info!(
        "downloading {:.1} MiB via P2P",
        file_len as f64 / 1_048_576.0
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

    let copied = tokio::io::copy(&mut recv, &mut file).await?;
    file.flush().await?;

    // The peer told us how many bytes to expect. If the stream ended early (or
    // ran long), the transfer is corrupt — and SHA verification may be skipped
    // when no digest is available, so never leave a truncated file on disk.
    if copied != file_len {
        drop(file);
        let _ = tokio::fs::remove_file(output).await;
        return Err(anyhow::anyhow!(
            "P2P transfer size mismatch: expected {file_len} bytes, got {copied}"
        ));
    }

    tracing::info!("P2P download complete");
    Ok(())
}
