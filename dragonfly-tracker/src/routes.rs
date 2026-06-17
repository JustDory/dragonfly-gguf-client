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

    if body.content_key.len() != 64 || body.node_id.is_empty() {
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
    let providers = store.get_peers(&query.content_key);
    tracing::debug!("returning {} providers for key {}", providers.len(), &query.content_key[..8.min(query.content_key.len())]);
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
