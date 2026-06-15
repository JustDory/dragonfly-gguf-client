/*
 *     Copyright 2026 The Dragonfly Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! GGUF backend implementation for downloading GGUF model files.
//!
//! This module provides support for the `gguf://` URL scheme. It is a thin wrapper
//! over the existing Hugging Face backend: it validates that the requested file has a
//! `.gguf` extension, rewrites the URL scheme `gguf://` -> `hf://`, and delegates the
//! actual `stat`/`get`/`put`/`exists` operations to an inner [`HuggingFace`] instance.
//! All Hugging Face options (revision, token, base URL, etc.) carry over unchanged.
//!
//! # URL Format
//!
//! The URL format mirrors the Hugging Face one but with the `gguf://` scheme:
//! `gguf://<repo_id>/<path>.gguf`
//!
//! Examples:
//! - `gguf://TheBloke/Llama-2-7B-GGUF/llama-2-7b.Q4_K_M.gguf`

use crate::{
    Backend, Body, ExistsRequest, GetRequest, GetResponse, PutRequest, PutResponse, StatRequest,
    StatResponse,
};
use async_trait::async_trait;
use dragonfly_client_config::dfdaemon::Config;
use dragonfly_client_core::{error::OrErr, error::ErrorType, Error, Result};
use std::sync::Arc;
use url::Url;

/// SCHEME is the URL scheme for the GGUF backend.
pub const SCHEME: &str = "gguf";

/// GGUF_EXTENSION is the required file extension for GGUF URLs (case-insensitive).
const GGUF_EXTENSION: &str = ".gguf";

/// GgufMetadata holds parsed metadata from a GGUF file header.
#[allow(dead_code)]
struct GgufMetadata {
    // TODO(metadata): parse GGUF header — arch, quant, n_params, context_len.
}

/// Gguf is the GGUF backend implementation. It is a thin wrapper over the
/// Hugging Face backend, delegating all I/O to an inner `HuggingFace` instance.
pub struct Gguf {
    /// Scheme is the scheme of the GGUF backend.
    scheme: String,

    /// Inner is the Hugging Face backend that performs the actual downloads.
    inner: crate::hugging_face::HuggingFace,
}

/// Gguf implements the GGUF backend.
impl Gguf {
    /// Create a new GGUF backend.
    pub fn new(config: Arc<Config>) -> Result<Self> {
        Ok(Self {
            scheme: SCHEME.to_string(),
            inner: crate::hugging_face::HuggingFace::new(config)?,
        })
    }

    /// Validates that the URL points to a `.gguf` file (case-insensitive),
    /// returning an `Unsupported` error otherwise.
    fn validate_gguf(url: &Url) -> Result<()> {
        if url
            .path()
            .to_ascii_lowercase()
            .ends_with(GGUF_EXTENSION)
        {
            Ok(())
        } else {
            Err(Error::Unsupported(format!(
                "gguf backend only supports *.gguf files, got: {url}"
            )))
        }
    }

    /// Rewrites a leading `gguf://` scheme to `hf://`, keeping the rest of the URL
    /// identical. Other schemes are rejected with an `InvalidURI` error.
    fn to_hf_url(raw: &str) -> Result<String> {
        let prefix = format!("{SCHEME}://");
        match raw.strip_prefix(prefix.as_str()) {
            Some(rest) => Ok(format!("{}://{}", crate::hugging_face::SCHEME, rest)),
            None => Err(Error::InvalidURI(raw.to_string())),
        }
    }

    /// Parses, validates the `.gguf` rule, and rewrites the raw `gguf://` url into
    /// an `hf://` url ready to be passed to the inner Hugging Face backend.
    fn rewrite_url(raw: &str) -> Result<String> {
        let url = Url::parse(raw).or_err(ErrorType::ParseError)?;
        Self::validate_gguf(&url)?;
        Self::to_hf_url(raw)
    }
}

// TODO(seed): GGUF preheat/seed-peer entrypoint will call get() here.

/// verify_hash validates the downloaded bytes against an expected digest.
#[allow(dead_code)]
fn verify_hash(_bytes: &[u8], _expected: Option<&str>) -> Result<()> {
    // TODO(hash): sha256 vs Hugging Face LFS oid.
    Ok(())
}

/// Backend implementation for GGUF.
#[async_trait]
impl Backend for Gguf {
    /// Scheme returns the scheme of the backend.
    fn scheme(&self) -> String {
        self.scheme.clone()
    }

    /// Stat the metadata from the backend.
    async fn stat(&self, mut request: StatRequest) -> Result<StatResponse> {
        request.url = Self::rewrite_url(&request.url)?;
        self.inner.stat(request).await
    }

    /// Get the content from the backend.
    async fn get(&self, mut request: GetRequest) -> Result<GetResponse<Body>> {
        request.url = Self::rewrite_url(&request.url)?;
        self.inner.get(request).await
    }

    /// Put the content to the backend.
    async fn put(&self, mut request: PutRequest) -> Result<PutResponse> {
        request.url = Self::rewrite_url(&request.url)?;
        self.inner.put(request).await
    }

    /// Exists checks whether the file exists in the backend.
    async fn exists(&self, mut request: ExistsRequest) -> Result<bool> {
        request.url = Self::rewrite_url(&request.url)?;
        self.inner.exists(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_hf_url() {
        assert_eq!(
            Gguf::to_hf_url("gguf://a/b/x.gguf").unwrap(),
            "hf://a/b/x.gguf"
        );
    }

    #[test]
    fn test_to_hf_url_rejects_other_scheme() {
        assert!(Gguf::to_hf_url("hf://a/b/x.gguf").is_err());
    }

    #[test]
    fn test_validate_gguf_accepts_gguf() {
        let url = Url::parse("gguf://a/b/x.gguf").unwrap();
        assert!(Gguf::validate_gguf(&url).is_ok());
    }

    #[test]
    fn test_validate_gguf_accepts_uppercase_extension() {
        let url = Url::parse("gguf://a/b/x.GGUF").unwrap();
        assert!(Gguf::validate_gguf(&url).is_ok());
    }

    #[test]
    fn test_validate_gguf_rejects_safetensors() {
        let url = Url::parse("gguf://a/b/x.safetensors").unwrap();
        assert!(Gguf::validate_gguf(&url).is_err());
    }

    #[test]
    fn test_validate_gguf_rejects_extensionless() {
        let url = Url::parse("gguf://a/b/x").unwrap();
        assert!(Gguf::validate_gguf(&url).is_err());
    }

    #[test]
    fn test_scheme() {
        let backend = Gguf::new(Arc::new(Config::default())).unwrap();
        assert_eq!(backend.scheme(), "gguf");
    }
}
