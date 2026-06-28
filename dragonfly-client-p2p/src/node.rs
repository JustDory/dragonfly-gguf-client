use anyhow::Result;
use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use std::path::Path;

pub struct IrohNode {
    pub(crate) endpoint: Endpoint,
}

impl IrohNode {
    pub async fn new(keypair_path: Option<&Path>) -> Result<Self> {
        let sk = match keypair_path {
            Some(path) => load_or_generate_secret_key(path).await?,
            None => SecretKey::generate(),
        };
        let endpoint = Endpoint::builder(presets::N0).secret_key(sk).bind().await?;
        Ok(Self { endpoint })
    }

    pub async fn close(self) {
        let _ = self.endpoint.close().await;
    }
}

async fn load_or_generate_secret_key(path: &Path) -> Result<SecretKey> {
    if path.exists() {
        let bytes = tokio::fs::read(path).await?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("keypair file must be exactly 32 bytes"))?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let sk = SecretKey::generate();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, sk.to_bytes()).await?;
    Ok(sk)
}
