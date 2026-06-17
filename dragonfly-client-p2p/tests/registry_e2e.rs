//! End-to-end test of the registry seed service:
//!   register_seed (write manifest)
//!     -> run_seed_service announces to a real in-process tracker
//!       -> try_p2p_download discovers the peer and pulls the file over Iroh.

use dragonfly_client_p2p as p2p;
use dragonfly_tracker::{routes, store};
use std::time::Duration;

#[tokio::test]
async fn registry_seed_service_serves_a_registered_file() {
    // 1. Start a real tracker on an ephemeral loopback port.
    let peer_store = store::PeerStore::new(1800, 10_000);
    let filter = routes::routes(peer_store);
    let (addr, server) = warp::serve(filter).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);
    let tracker_url = format!("http://127.0.0.1:{}", addr.port());

    // 2. Lay down a model file and register it for seeding.
    let dir = tempfile::tempdir().unwrap();
    let registry = dir.path().join("registry");
    let model = dir.path().join("model.gguf");
    let out = dir.path().join("downloaded.gguf");
    let content = b"GGUF registry end-to-end content";
    tokio::fs::write(&model, content).await.unwrap();

    let key = p2p::content_key("gguf://owner/repo/model.gguf", "main");
    p2p::register_seed(
        &registry,
        &tracker_url,
        &key,
        &model,
        Duration::from_secs(60),
    )
    .unwrap();

    // 3. Run the seed service (the job dfdaemon does). interval() fires its
    //    first tick immediately, so the announce lands without a 30s wait.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let registry_for_svc = registry.clone();
    let svc = tokio::spawn(async move {
        p2p::run_seed_service(registry_for_svc, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    // 4. Download via the public P2P entry point, retrying briefly to let the
    //    first reconcile + announce propagate to the tracker.
    let mut last_err = None;
    let mut ok = false;
    for _ in 0..20 {
        match p2p::try_p2p_download(&tracker_url, &key, &out, None, Duration::from_secs(10)).await {
            Ok(()) => {
                ok = true;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    // 5. Shut the service down cleanly before asserting.
    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), svc).await;

    assert!(ok, "P2P download never succeeded: {last_err:?}");
    let downloaded = tokio::fs::read(&out).await.unwrap();
    assert_eq!(downloaded.as_slice(), content, "downloaded bytes mismatch");
}
