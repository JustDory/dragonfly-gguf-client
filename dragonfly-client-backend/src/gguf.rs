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
//!
//! # Cache / task-id stability
//!
//! The daemon derives a task id from the original `gguf://` URL *before* this backend
//! rewrites it, so two peers requesting the same `gguf://` URL resolve to the same task
//! and share pieces over P2P. [`Gguf::rewrite_url`] is therefore deterministic and
//! preserves the full path and query string. (Note: the Hugging Face revision is carried
//! as a separate request option rather than in the URL, so distinguishing revisions for
//! task-id purposes is a daemon-level concern, not handled here.)

use crate::{
    Backend, Body, ExistsRequest, GetRequest, GetResponse, PutRequest, PutResponse, StatRequest,
    StatResponse,
};
use async_trait::async_trait;
use dragonfly_api::common::v2::Range;
use dragonfly_client_config::dfdaemon::Config;
use dragonfly_client_core::{
    error::{ErrorType, OrErr},
    Error, Result,
};
use dragonfly_client_util::digest::{verify_file_digest, Algorithm, Digest};
use reqwest::header::{HeaderMap, ETAG};
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use url::Url;

/// SCHEME is the URL scheme for the GGUF backend.
pub const SCHEME: &str = "gguf";

/// GGUF_EXTENSION is the required file extension for GGUF URLs (case-insensitive).
const GGUF_EXTENSION: &str = ".gguf";

/// GGUF_MAGIC is the 4-byte magic that every GGUF file starts with.
const GGUF_MAGIC: &[u8] = b"GGUF";

/// HEADER_FETCH_LEN is how many leading bytes to range-request when reading GGUF
/// header metadata without downloading the whole file (1 MiB comfortably covers
/// the header of typical models).
const HEADER_FETCH_LEN: u64 = 1 << 20;

/// GGUF metadata value type tags (see the GGUF specification).
const GGUF_TYPE_UINT32: u32 = 4;
const GGUF_TYPE_STRING: u32 = 8;
const GGUF_TYPE_ARRAY: u32 = 9;

/// GgufMetadata holds metadata parsed from a GGUF file header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GgufMetadata {
    /// version is the GGUF format version.
    pub version: u32,

    /// tensor_count is the number of tensors declared in the file.
    pub tensor_count: u64,

    /// metadata_kv_count is the number of metadata key/value pairs declared.
    pub metadata_kv_count: u64,

    /// architecture is the value of `general.architecture` (e.g. "llama"), if present.
    pub architecture: Option<String>,

    /// name is the value of `general.name`, if present.
    pub name: Option<String>,

    /// file_type is the value of `general.file_type` (the quantization enum), if present.
    pub file_type: Option<u32>,
}

/// GgufReader is a minimal little-endian cursor over a GGUF byte buffer. Every read is
/// bounds-checked and returns `None` when the buffer is exhausted, which lets callers
/// stop gracefully when only a partial header was fetched.
struct GgufReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> GgufReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// take advances the cursor by `n` bytes and returns the slice, or `None` if there
    /// are fewer than `n` bytes remaining.
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn read_u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_u64(&mut self) -> Option<u64> {
        let b = self.take(8)?;
        Some(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// read_string reads a GGUF string (u64 length prefix followed by UTF-8 bytes).
    fn read_string(&mut self) -> Option<String> {
        let len = self.read_u64()? as usize;
        let b = self.take(len)?;
        Some(String::from_utf8_lossy(b).into_owned())
    }

    /// skip_value advances past a metadata value of the given type without decoding it.
    fn skip_value(&mut self, value_type: u32) -> Option<()> {
        match value_type {
            // uint8, int8, bool.
            0..=1 | 7 => {
                self.take(1)?;
            }
            // uint16, int16.
            2..=3 => {
                self.take(2)?;
            }
            // uint32, int32, float32.
            4..=6 => {
                self.take(4)?;
            }
            // uint64, int64, float64.
            10..=12 => {
                self.take(8)?;
            }
            // string.
            GGUF_TYPE_STRING => {
                let len = self.read_u64()? as usize;
                self.take(len)?;
            }
            // array of values.
            GGUF_TYPE_ARRAY => {
                let elem_type = self.read_u32()?;
                let len = self.read_u64()?;
                for _ in 0..len {
                    self.skip_value(elem_type)?;
                }
            }
            // Unknown type: we can no longer reason about the layout, so stop.
            _ => return None,
        }
        Some(())
    }
}

impl GgufMetadata {
    /// parse reads the fixed GGUF header (magic, version, counts) and scans the
    /// metadata key/value section for the well-known `general.*` keys. If the buffer is
    /// truncated part-way through the key/value section (e.g. only the file header was
    /// fetched), parsing stops gracefully and returns whatever was captured so far.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut reader = GgufReader::new(data);

        let magic = reader
            .take(4)
            .ok_or_else(|| Error::ValidationError("gguf header too short".to_string()))?;
        if magic != GGUF_MAGIC {
            return Err(Error::ValidationError(format!(
                "not a GGUF file: unexpected magic {magic:02x?}"
            )));
        }

        let version = reader
            .read_u32()
            .ok_or_else(|| Error::ValidationError("gguf header truncated: version".to_string()))?;
        let tensor_count = reader.read_u64().ok_or_else(|| {
            Error::ValidationError("gguf header truncated: tensor_count".to_string())
        })?;
        let metadata_kv_count = reader.read_u64().ok_or_else(|| {
            Error::ValidationError("gguf header truncated: metadata_kv_count".to_string())
        })?;

        let mut metadata = GgufMetadata {
            version,
            tensor_count,
            metadata_kv_count,
            ..Default::default()
        };

        for _ in 0..metadata_kv_count {
            let Some(key) = reader.read_string() else {
                break;
            };
            let Some(value_type) = reader.read_u32() else {
                break;
            };

            match (key.as_str(), value_type) {
                ("general.architecture", GGUF_TYPE_STRING) => {
                    let Some(v) = reader.read_string() else {
                        break;
                    };
                    metadata.architecture = Some(v);
                }
                ("general.name", GGUF_TYPE_STRING) => {
                    let Some(v) = reader.read_string() else {
                        break;
                    };
                    metadata.name = Some(v);
                }
                ("general.file_type", GGUF_TYPE_UINT32) => {
                    let Some(v) = reader.read_u32() else {
                        break;
                    };
                    metadata.file_type = Some(v);
                }
                _ => {
                    if reader.skip_value(value_type).is_none() {
                        break;
                    }
                }
            }
        }

        Ok(metadata)
    }
}

/// expected_sha256 extracts the sha256 (hex) that Hugging Face advertises for a file.
/// For LFS-backed files (which all real GGUF models are), HF returns the object's
/// sha256 in the `X-Linked-Etag` header; for small non-LFS files it is the `ETag`.
/// The value is quoted and may carry a weak-validator `W/` prefix. Returns `None` if
/// no header looks like a sha256.
pub fn expected_sha256(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get("x-linked-etag")
        .or_else(|| headers.get(ETAG))?
        .to_str()
        .ok()?;

    let cleaned = raw.trim().trim_start_matches("W/").trim_matches('"');
    if cleaned.len() == 64 && cleaned.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(cleaned.to_ascii_lowercase())
    } else {
        None
    }
}

/// verify_gguf_digest verifies a downloaded GGUF file at `path` against the expected
/// sha256 (hex), reusing the client's shared digest utilities. This is the integration
/// point for post-download integrity checking (and seed-peer preheat verification).
pub fn verify_gguf_digest(expected_sha256: &str, path: &Path) -> Result<()> {
    verify_file_digest(
        Digest::new(Algorithm::Sha256, expected_sha256.to_string()),
        path,
    )
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
        if url.path().to_ascii_lowercase().ends_with(GGUF_EXTENSION) {
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
    ///
    /// This is deterministic: the same `gguf://` input always produces the same
    /// `hf://` output (preserving path and query), which keeps task ids stable across
    /// peers so the same model is shared over P2P.
    fn rewrite_url(raw: &str) -> Result<String> {
        let url = Url::parse(raw).or_err(ErrorType::ParseError)?;
        Self::validate_gguf(&url)?;
        Self::to_hf_url(raw)
    }

    /// fetch_header_metadata range-requests the first [`HEADER_FETCH_LEN`] bytes of the
    /// GGUF file via the Hugging Face backend and parses the header metadata, without
    /// downloading the whole model. This is the entry point future preheat/validation
    /// flows can use to inspect a model before fully fetching it.
    pub async fn fetch_header_metadata(&self, mut request: GetRequest) -> Result<GgufMetadata> {
        request.url = Self::rewrite_url(&request.url)?;
        request.range = Some(Range {
            start: 0,
            length: HEADER_FETCH_LEN,
        });

        let response = self.inner.get(request).await?;
        if !response.success {
            return Err(Error::Unsupported(
                response
                    .error_message
                    .unwrap_or_else(|| "failed to fetch GGUF header".to_string()),
            ));
        }

        let mut buffer = Vec::new();
        response
            .reader
            .take(HEADER_FETCH_LEN)
            .read_to_end(&mut buffer)
            .await?;

        GgufMetadata::parse(&buffer)
    }
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
    use reqwest::header::HeaderValue;

    // --- URL handling (#6: deterministic rewrite -> stable task ids) ---

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

    #[test]
    fn test_rewrite_url_deterministic_and_preserves_path() {
        let a = Gguf::rewrite_url("gguf://owner/repo/sub/model.gguf").unwrap();
        let b = Gguf::rewrite_url("gguf://owner/repo/sub/model.gguf").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "hf://owner/repo/sub/model.gguf");
    }

    #[test]
    fn test_rewrite_url_preserves_query() {
        assert_eq!(
            Gguf::rewrite_url("gguf://owner/repo/model.gguf?revision=v2").unwrap(),
            "hf://owner/repo/model.gguf?revision=v2"
        );
    }

    #[test]
    fn test_rewrite_url_rejects_non_gguf() {
        assert!(Gguf::rewrite_url("gguf://owner/repo/model.txt").is_err());
    }

    // --- GGUF metadata parsing (#2) ---

    fn put_str(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }

    fn put_kv_string(buf: &mut Vec<u8>, key: &str, val: &str) {
        put_str(buf, key);
        buf.extend_from_slice(&GGUF_TYPE_STRING.to_le_bytes());
        put_str(buf, val);
    }

    fn put_kv_u32(buf: &mut Vec<u8>, key: &str, v: u32) {
        put_str(buf, key);
        buf.extend_from_slice(&GGUF_TYPE_UINT32.to_le_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn put_kv_array_u32(buf: &mut Vec<u8>, key: &str, vals: &[u32]) {
        put_str(buf, key);
        buf.extend_from_slice(&GGUF_TYPE_ARRAY.to_le_bytes());
        buf.extend_from_slice(&GGUF_TYPE_UINT32.to_le_bytes());
        buf.extend_from_slice(&(vals.len() as u64).to_le_bytes());
        for v in vals {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }

    fn gguf_header(kv_count: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"GGUF");
        buf.extend_from_slice(&3u32.to_le_bytes()); // version
        buf.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        buf.extend_from_slice(&kv_count.to_le_bytes());
        buf
    }

    #[test]
    fn test_parse_gguf_metadata() {
        let mut buf = gguf_header(3);
        put_kv_string(&mut buf, "general.architecture", "llama");
        put_kv_string(&mut buf, "general.name", "test-model");
        put_kv_u32(&mut buf, "general.file_type", 15);

        let md = GgufMetadata::parse(&buf).unwrap();
        assert_eq!(md.version, 3);
        assert_eq!(md.tensor_count, 0);
        assert_eq!(md.metadata_kv_count, 3);
        assert_eq!(md.architecture.as_deref(), Some("llama"));
        assert_eq!(md.name.as_deref(), Some("test-model"));
        assert_eq!(md.file_type, Some(15));
    }

    #[test]
    fn test_parse_skips_unknown_and_array_values() {
        let mut buf = gguf_header(3);
        put_kv_array_u32(&mut buf, "tokenizer.ggml.tokens", &[10, 20, 30]);
        put_kv_string(&mut buf, "general.architecture", "qwen2");
        put_kv_string(&mut buf, "general.name", "after-array");

        let md = GgufMetadata::parse(&buf).unwrap();
        assert_eq!(md.architecture.as_deref(), Some("qwen2"));
        assert_eq!(md.name.as_deref(), Some("after-array"));
    }

    #[test]
    fn test_parse_rejects_bad_magic() {
        let data = b"NOPE\x03\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        assert!(GgufMetadata::parse(data).is_err());
    }

    #[test]
    fn test_parse_rejects_truncated_header() {
        // Magic + version only, no tensor/kv counts.
        let data = b"GGUF\x03\x00\x00\x00";
        assert!(GgufMetadata::parse(data).is_err());
    }

    #[test]
    fn test_parse_tolerates_truncated_kv_section() {
        // Declares 5 KV pairs but only provides one before the buffer ends.
        let mut buf = gguf_header(5);
        put_kv_string(&mut buf, "general.architecture", "llama");

        let md = GgufMetadata::parse(&buf).unwrap();
        assert_eq!(md.metadata_kv_count, 5);
        assert_eq!(md.architecture.as_deref(), Some("llama"));
        assert_eq!(md.name, None);
    }

    // --- Hash verification (#1) ---

    #[test]
    fn test_expected_sha256_from_x_linked_etag() {
        let sha = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-linked-etag",
            HeaderValue::from_str(&format!("\"{sha}\"")).unwrap(),
        );
        assert_eq!(expected_sha256(&headers).as_deref(), Some(sha));
    }

    #[test]
    fn test_expected_sha256_prefers_linked_over_etag() {
        let linked = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, HeaderValue::from_static("\"deadbeef\""));
        headers.insert(
            "x-linked-etag",
            HeaderValue::from_str(&format!("\"{linked}\"")).unwrap(),
        );
        assert_eq!(expected_sha256(&headers).as_deref(), Some(linked));
    }

    #[test]
    fn test_expected_sha256_none_for_non_hash_etag() {
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, HeaderValue::from_static("\"not-a-sha\""));
        assert_eq!(expected_sha256(&headers), None);
    }

    #[test]
    fn test_verify_gguf_digest_ok_and_mismatch() {
        use std::io::Write;

        let sha_hello = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"hello").unwrap();
        file.flush().unwrap();

        assert!(verify_gguf_digest(sha_hello, file.path()).is_ok());

        let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(verify_gguf_digest(wrong, file.path()).is_err());
    }
}
