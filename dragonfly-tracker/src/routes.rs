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
    /// Optional content metadata (filename, format, size). Pre-metadata
    /// clients simply omit these; the tracker treats the key as anonymous.
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    size: Option<u64>,
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

#[derive(Debug, Deserialize)]
struct ContentsQuery {
    /// Case-insensitive exact match on the announced format (e.g. "gguf",
    /// "safetensors") — the "category" filter for registry UIs.
    format: Option<String>,
    /// Case-insensitive substring search on the announced filename.
    q: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ContentsResponse {
    contents: Vec<crate::store::ContentSummary>,
}

/// Default and hard cap for `GET /contents` page size.
const DEFAULT_CONTENTS_LIMIT: usize = 100;
const MAX_CONTENTS_LIMIT: usize = 500;

/// Upper bound on a serialized Iroh `EndpointAddr` we are willing to store. Real
/// records are well under 1 KiB; this keeps a misbehaving client from bloating
/// the in-memory store with huge announcements.
const MAX_ADDR_INFO_LEN: usize = 8 * 1024;

/// Upper bound on an Iroh node id (a base32-encoded ed25519 key is ~52 chars).
const MAX_NODE_ID_LEN: usize = 128;

/// Upper bounds on announced content metadata. Filenames on every mainstream
/// filesystem cap at 255 bytes; formats are short extensions. These keep a
/// misbehaving client from bloating the in-memory metadata map.
const MAX_FILENAME_LEN: usize = 512;
const MAX_FORMAT_LEN: usize = 32;

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
        && valid_meta(body)
}

/// Validates the optional metadata fields: when present they must be
/// non-empty, bounded, and free of control characters (they are echoed back
/// verbatim by `GET /contents`, so keep what we store display-safe).
fn valid_meta(body: &AnnounceBody) -> bool {
    let ok_str =
        |s: &str, max: usize| !s.is_empty() && s.len() <= max && !s.chars().any(|c| c.is_control());
    body.filename
        .as_deref()
        .is_none_or(|f| ok_str(f, MAX_FILENAME_LEN))
        && body
            .format
            .as_deref()
            .is_none_or(|f| ok_str(f, MAX_FORMAT_LEN))
}

/// Extracts the optional content metadata from an announce body, if any field
/// was supplied.
fn announce_meta(body: &AnnounceBody) -> Option<crate::store::ContentMeta> {
    if body.filename.is_none() && body.format.is_none() && body.size.is_none() {
        return None;
    }
    Some(crate::store::ContentMeta {
        filename: body.filename.clone(),
        format: body.format.clone(),
        size: body.size,
    })
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

    let contents = warp::get()
        .and(warp::path("contents"))
        .and(warp::path::end())
        .and(warp::query::<ContentsQuery>())
        .and(with_store(store.clone()))
        .and_then(handle_contents);

    announce.or(peers).or(leave).or(contents)
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

    let meta = announce_meta(&body);
    store.announce(body.content_key, body.node_id, body.addr_info, meta);
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

async fn handle_contents(
    query: ContentsQuery,
    store: Arc<PeerStore>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_CONTENTS_LIMIT)
        .min(MAX_CONTENTS_LIMIT);
    let contents = store.list_contents(query.format.as_deref(), query.q.as_deref(), limit);
    tracing::debug!("returning {} contents", contents.len());
    Ok(warp::reply::with_status(
        warp::reply::json(&ContentsResponse { contents }),
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
            filename: None,
            format: None,
            size: None,
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
    fn accepts_announce_with_metadata() {
        let mut b = body(&"a".repeat(64), "node-1", "{}");
        b.filename = Some("model.safetensors".to_string());
        b.format = Some("safetensors".to_string());
        b.size = Some(1234);
        assert!(valid_announce(&b));
        let meta = announce_meta(&b).unwrap();
        assert_eq!(meta.filename.as_deref(), Some("model.safetensors"));
        assert_eq!(meta.format.as_deref(), Some("safetensors"));
        assert_eq!(meta.size, Some(1234));
    }

    #[test]
    fn rejects_malformed_metadata() {
        // Empty strings, oversized values, and control characters are refused;
        // fully absent metadata stays valid (pre-metadata clients).
        let mut b = body(&"a".repeat(64), "node-1", "{}");
        b.filename = Some(String::new());
        assert!(!valid_announce(&b));

        b.filename = Some("f".repeat(MAX_FILENAME_LEN + 1));
        assert!(!valid_announce(&b));

        b.filename = Some("evil\nname.gguf".to_string());
        assert!(!valid_announce(&b));

        b.filename = None;
        b.format = Some("f".repeat(MAX_FORMAT_LEN + 1));
        assert!(!valid_announce(&b));

        b.format = None;
        assert!(valid_announce(&b));
        assert!(announce_meta(&b).is_none());

        // Size alone is enough to carry metadata.
        b.size = Some(7);
        assert!(valid_announce(&b));
        assert!(announce_meta(&b).is_some());
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
