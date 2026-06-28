use crate::store::PeerStore;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::Arc;
use warp::Filter;

#[derive(Debug, Deserialize)]
struct AnnounceBody {
    content_key: String,
    node_id: String,
    addr_info: String,
}

#[derive(Debug, Deserialize)]
struct LeaveBody {
    content_key: String,
    node_id: String,
}

#[derive(Debug, Deserialize)]
struct PeersQuery {
    content_key: String,
}

#[derive(Debug, Serialize)]
struct PeersResponse {
    providers: Vec<crate::store::PeerEntry>,
}

/// Upper bound on a serialized Iroh `EndpointAddr` we are willing to store. Real
/// records are well under 1 KiB; this keeps a misbehaving client from bloating
/// the in-memory store with huge announcements.
const MAX_ADDR_INFO_LEN: usize = 8 * 1024;

/// Upper bound on an Iroh node id (a base32-encoded ed25519 key is ~52 chars).
const MAX_NODE_ID_LEN: usize = 128;

/// Validates an announce body before it is stored: the content key must be a
/// 64-char hex sha256, the node id must be a sane non-empty string, and addr_info
/// must be non-empty, bounded, and valid JSON (it is a serialized Iroh
/// `EndpointAddr`). Rejecting malformed records here keeps the peer store small
/// and the data we hand back to downloaders well-formed.
fn valid_announce(body: &AnnounceBody) -> bool {
    body.content_key.len() == 64
        && body.content_key.bytes().all(|b| b.is_ascii_hexdigit())
        && !body.node_id.is_empty()
        && body.node_id.len() <= MAX_NODE_ID_LEN
        && !body.addr_info.is_empty()
        && body.addr_info.len() <= MAX_ADDR_INFO_LEN
        && serde_json::from_str::<serde_json::Value>(&body.addr_info).is_ok()
}

fn with_store(
    store: Arc<PeerStore>,
) -> impl Filter<Extract = (Arc<PeerStore>,), Error = Infallible> + Clone {
    warp::any().map(move || store.clone())
}

pub fn routes(
    store: Arc<PeerStore>,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    let announce = warp::post()
        .and(warp::path("announce"))
        .and(warp::path::end())
        .and(warp::addr::remote())
        .and(warp::body::json::<AnnounceBody>())
        .and(with_store(store.clone()))
        .and_then(handle_announce);

    let peers = warp::get()
        .and(warp::path("peers"))
        .and(warp::path::end())
        .and(warp::query::<PeersQuery>())
        .and(with_store(store.clone()))
        .and_then(handle_peers);

    let leave = warp::delete()
        .and(warp::path("leave"))
        .and(warp::path::end())
        .and(warp::body::json::<LeaveBody>())
        .and(with_store(store.clone()))
        .and_then(handle_leave);

    announce.or(peers).or(leave)
}

async fn handle_announce(
    remote_addr: Option<std::net::SocketAddr>,
    body: AnnounceBody,
    store: Arc<PeerStore>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let ip = remote_addr
        .map(|a| a.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));

    if !store.check_rate_limit(ip) {
        return Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": "rate limited"})),
            warp::http::StatusCode::TOO_MANY_REQUESTS,
        ));
    }

    if !valid_announce(&body) {
        return Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": "invalid request"})),
            warp::http::StatusCode::BAD_REQUEST,
        ));
    }

    store.announce(body.content_key, body.node_id, body.addr_info);
    tracing::debug!("announced peer");

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({"ok": true})),
        warp::http::StatusCode::OK,
    ))
}

async fn handle_peers(
    query: PeersQuery,
    store: Arc<PeerStore>,
) -> Result<impl warp::Reply, warp::Rejection> {
    if query.content_key.len() != 64 || !query.content_key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": "invalid content_key"})),
            warp::http::StatusCode::BAD_REQUEST,
        ));
    }
    let providers = store.get_peers(&query.content_key);
    tracing::debug!(
        "returning {} providers for key {}",
        providers.len(),
        &query.content_key[..8.min(query.content_key.len())]
    );
    Ok(warp::reply::with_status(
        warp::reply::json(&PeersResponse { providers }),
        warp::http::StatusCode::OK,
    ))
}

async fn handle_leave(
    body: LeaveBody,
    store: Arc<PeerStore>,
) -> Result<impl warp::Reply, warp::Rejection> {
    store.remove_peer(&body.content_key, &body.node_id);
    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({"ok": true})),
        warp::http::StatusCode::OK,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(key: &str, node: &str, addr: &str) -> AnnounceBody {
        AnnounceBody {
            content_key: key.to_string(),
            node_id: node.to_string(),
            addr_info: addr.to_string(),
        }
    }

    #[test]
    fn accepts_well_formed_announce() {
        assert!(valid_announce(&body(&"a".repeat(64), "node-1", "{}")));
    }

    #[test]
    fn rejects_malformed_announce() {
        // Wrong content_key length or non-hex characters.
        assert!(!valid_announce(&body(&"a".repeat(63), "node", "{}")));
        assert!(!valid_announce(&body(&"z".repeat(64), "node", "{}")));
        // Empty / oversized node id.
        assert!(!valid_announce(&body(&"a".repeat(64), "", "{}")));
        assert!(!valid_announce(&body(
            &"a".repeat(64),
            &"n".repeat(MAX_NODE_ID_LEN + 1),
            "{}"
        )));
        // Empty, non-JSON, or oversized addr_info.
        assert!(!valid_announce(&body(&"a".repeat(64), "node", "")));
        assert!(!valid_announce(&body(&"a".repeat(64), "node", "not json")));
        let huge = format!("\"{}\"", "x".repeat(MAX_ADDR_INFO_LEN));
        assert!(!valid_announce(&body(&"a".repeat(64), "node", &huge)));
    }

    #[test]
    fn peers_query_key_validation() {
        // Mirrors the content_key check in handle_peers: only 64 lowercase hex accepted.
        let valid_key = "a".repeat(64);
        assert_eq!(valid_key.len(), 64);
        assert!(valid_key.bytes().all(|b| b.is_ascii_hexdigit()));

        let short = "a".repeat(63);
        assert!(short.len() != 64 || !short.bytes().all(|b| b.is_ascii_hexdigit()));

        let non_hex = "z".repeat(64);
        assert!(!non_hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
