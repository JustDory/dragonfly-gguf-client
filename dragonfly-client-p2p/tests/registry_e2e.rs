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

    let key = p2p::content_key("gguf://owner/repo/model.gguf", "main", None);
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

/// End-to-end for the local-model scanner: plant a Hugging Face hub-cache tree,
/// let the offline scanner reconstruct its content key, register + serve it, and
/// pull it back over Iroh by the key a downloader would compute.
#[tokio::test]
async fn scanner_discovers_and_serves_a_hub_cache_model() {
    // 1. Start a real tracker on an ephemeral loopback port.
    let peer_store = store::PeerStore::new(1800, 10_000);
    let filter = routes::routes(peer_store);
    let (addr, server) = warp::serve(filter).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);
    let tracker_url = format!("http://127.0.0.1:{}", addr.port());

    // 2. Plant a hub-cache tree: models--owner--repo/snapshots/<commit>/model.gguf
    //    with refs/main -> <commit>.
    let dir = tempfile::tempdir().unwrap();
    let hub = dir.path().join("hub");
    let commit = "abc123def456";
    let repo = hub.join("models--owner--repo");
    let snap = repo.join("snapshots").join(commit);
    tokio::fs::create_dir_all(&snap).await.unwrap();
    let content = b"GGUF hub-cache scanner end-to-end content";
    tokio::fs::write(snap.join("model.gguf"), content)
        .await
        .unwrap();
    tokio::fs::create_dir_all(repo.join("refs")).await.unwrap();
    tokio::fs::write(repo.join("refs").join("main"), commit)
        .await
        .unwrap();

    // 3. Discover through the offline scanner (HF_HUB_CACHE points at our tree).
    //    set/remove bracket a synchronous call, so no env leaks across an await.
    std::env::set_var("HF_HUB_CACHE", &hub);
    let cfg = p2p::ScanConfig {
        tracker_url: tracker_url.clone(),
        min_size: 0,
        resolve: false,
        ..Default::default()
    };
    let seeds = p2p::discover_seeds(&cfg);
    std::env::remove_var("HF_HUB_CACHE");

    let want = p2p::content_key("hf://owner/repo/model.gguf", "main", None);
    let seed = seeds
        .iter()
        .find(|s| s.content_keys.contains(&want))
        .expect("hub-cache model should be discovered");

    // 4. Register every reconstructed key (the job run_scan_service does) and
    //    run the seed service that serves + announces them.
    let registry = dir.path().join("registry");
    for key in &seed.content_keys {
        p2p::register_seed(
            &registry,
            &tracker_url,
            key,
            &seed.file_path,
            Duration::from_secs(60),
        )
        .unwrap();
    }
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let registry_for_svc = registry.clone();
    let svc = tokio::spawn(async move {
        p2p::run_seed_service(registry_for_svc, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    // 5. Download by the key a `gguf://owner/repo/model.gguf` downloader computes.
    let out = dir.path().join("downloaded.gguf");
    let mut last_err = None;
    let mut ok = false;
    for _ in 0..20 {
        match p2p::try_p2p_download(&tracker_url, &want, &out, None, Duration::from_secs(10)).await
        {
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

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), svc).await;

    assert!(
        ok,
        "P2P download of discovered model never succeeded: {last_err:?}"
    );
    assert_eq!(
        tokio::fs::read(&out).await.unwrap().as_slice(),
        content,
        "downloaded bytes mismatch"
    );
}
