//! Local model-directory scanner.
//!
//! Popular local inference engines (the shared Hugging Face hub cache used by
//! transformers / vLLM / TGI / modern `llama.cpp -hf`, plus LM Studio, the
//! legacy llama.cpp cache, Jan, Ollama, GPT4All, ...) leave gigabytes of
//! already-downloaded models on disk. This module walks their well-known
//! directories, maps each pre-downloaded model back to its Hugging Face
//! content key, and registers it as a P2P seed — so those models are shared
//! over the swarm without ever being re-downloaded.
//!
//! A seed is only useful if its content key matches what a downloader computes
//! (`content_key = sha256("hf://owner/repo/path:revision")`, see
//! [`crate::content_key`]). We therefore only ever register a file once its
//! exact Hugging Face coordinates are known — either because the on-disk layout
//! preserves them (the offline mappers here) or because a forward name lookup
//! on Hugging Face confirms them by hash (the `resolver` submodule).

use crate::content_key;
use crate::seeder::register_seed;
use crate::DEFAULT_TRACKER_URL;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Model-weight extensions seeded by default (lowercased, no leading dot).
pub const DEFAULT_FORMATS: &[&str] = &["gguf", "safetensors"];

/// Skip files smaller than this. A hub cache holds thousands of tiny config /
/// tokenizer files that are pointless to announce (and carry no LFS sha256 to
/// verify against). 1 MiB by default.
pub const DEFAULT_MIN_SIZE: u64 = 1 << 20;

/// TTL applied to a discovered seed manifest. Comfortably longer than
/// [`SCAN_INTERVAL`] so live models never flap between scans, yet bounded so a
/// model deleted out from under us eventually ages out even if the reconcile
/// loop's `exists()` check somehow misses it.
const DEFAULT_SEED_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// How often the scan service re-walks the configured directories.
const SCAN_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Upper bound on directory-walk recursion — guards against symlink loops and
/// pathological trees (hub-cache snapshots are symlinks, which we follow).
const MAX_WALK_DEPTH: usize = 10;

/// Configuration for a local-model scan.
#[derive(Clone, Debug)]
pub struct ScanConfig {
    /// Tracker discovered seeds are announced to. Written into each manifest;
    /// the seed service (`run_seed_service`) does the actual announcing.
    pub tracker_url: String,
    /// Extra roots to scan for loose model files, on top of the built-in engine
    /// locations (e.g. from repeated `--gguf-scan-dir`). These are treated as
    /// loose-file roots and only mapped via the network resolver.
    pub extra_dirs: Vec<PathBuf>,
    /// Lowercased extensions (no dot) that count as model files.
    pub formats: Vec<String>,
    /// Minimum file size in bytes.
    pub min_size: u64,
    /// TTL applied to each discovered seed manifest.
    pub ttl: Duration,
    /// Whether the network name-search resolver runs (Ollama + loose files).
    pub resolve: bool,
    /// Optional Hugging Face token for authenticated API lookups.
    pub hf_token: Option<String>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            tracker_url: DEFAULT_TRACKER_URL.to_string(),
            extra_dirs: Vec::new(),
            formats: DEFAULT_FORMATS.iter().map(|s| s.to_string()).collect(),
            min_size: DEFAULT_MIN_SIZE,
            ttl: DEFAULT_SEED_TTL,
            resolve: true,
            hf_token: hf_token_from_env(),
        }
    }
}

fn hf_token_from_env() -> Option<String> {
    std::env::var("HF_TOKEN")
        .or_else(|_| std::env::var("HUGGING_FACE_HUB_TOKEN"))
        .ok()
        .filter(|t| !t.is_empty())
}

/// A local model file mapped to the content key(s) it can serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredSeed {
    /// Content keys this file satisfies. A hub-cache file, for example, is
    /// reachable both as `main` and as its commit hash, so it has several.
    pub content_keys: Vec<String>,
    /// Absolute path to the local model file.
    pub file_path: PathBuf,
    /// Which engine/source the file was discovered in (for logging).
    pub source: &'static str,
}

/// A reconstructed source coordinate: a source URL plus the revision string a
/// downloader would pass.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceCoord {
    /// Full source URL, e.g. `hf://owner/repo/path.gguf`.
    url: String,
    /// Revision exactly as a downloader passes it (e.g. `main`, or a commit).
    revision: String,
}

// ---------------------------------------------------------------------------
// Engine directory resolution
// ---------------------------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn path_from_env(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Hugging Face hub cache roots: `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else
/// `~/.cache/huggingface/hub`. Shared by transformers, vLLM, TGI,
/// sentence-transformers, `huggingface-cli`, text-embeddings-inference and
/// modern `llama.cpp -hf`. `$LLAMA_CACHE` is included because recent llama.cpp
/// points it at a hub-layout directory.
fn hf_hub_cache_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = path_from_env("HF_HUB_CACHE") {
        out.push(p);
    }
    if let Some(p) = path_from_env("HF_HOME") {
        out.push(p.join("hub"));
    }
    if let Some(p) = path_from_env("LLAMA_CACHE") {
        out.push(p);
    }
    if let Some(home) = home_dir() {
        out.push(home.join(".cache").join("huggingface").join("hub"));
    }
    dedup_paths(out)
}

/// LM Studio model roots: `~/.lmstudio/models` (and the legacy
/// `~/.cache/lm-studio/models`). Layout: `<owner>/<repo>/<file>`.
fn lmstudio_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        out.push(home.join(".lmstudio").join("models"));
        out.push(home.join(".cache").join("lm-studio").join("models"));
    }
    dedup_paths(out)
}

/// Legacy llama.cpp cache roots (pre hub-cache consolidation): flat directories
/// of `<owner>_<repo>_<file>.gguf` files each with a `.json` metadata sidecar.
fn llamacpp_legacy_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = path_from_env("LLAMA_CACHE") {
        out.push(p);
    }
    if let Some(home) = home_dir() {
        out.push(home.join(".cache").join("llama.cpp"));
    }
    dedup_paths(out)
}

/// Jan model roots. Layout: `<root>/<model_id>/model.json` + weight files.
fn jan_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = path_from_env("JAN_DATA_FOLDER") {
        out.push(p.join("models"));
    }
    if let Some(home) = home_dir() {
        out.push(home.join("jan").join("models"));
        out.push(home.join(".jan").join("models"));
    }
    dedup_paths(out)
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// Recursively invoke `cb(path, size)` for every regular file under `root`,
/// following symlinks (the hub cache symlinks snapshot files into `blobs/`),
/// bounded by [`MAX_WALK_DEPTH`]. Missing/unreadable directories are skipped.
fn walk_files(root: &Path, mut cb: impl FnMut(&Path, u64)) {
    fn inner(dir: &Path, depth: usize, cb: &mut dyn FnMut(&Path, u64)) {
        if depth > MAX_WALK_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // metadata() follows symlinks so snapshot links resolve to blobs.
            let Ok(md) = std::fs::metadata(&path) else {
                continue;
            };
            if md.is_dir() {
                inner(&path, depth + 1, cb);
            } else if md.is_file() {
                cb(&path, md.len());
            }
        }
    }
    inner(root, 0, &mut cb);
}

/// Lowercased extension of `path` if it is one of the configured model formats.
fn model_ext(path: &Path, formats: &[String]) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    formats.iter().any(|f| f == &ext).then_some(ext)
}

fn is_model_file(path: &Path, size: u64, cfg: &ScanConfig) -> bool {
    size >= cfg.min_size && model_ext(path, &cfg.formats).is_some()
}

// ---------------------------------------------------------------------------
// URL / content-key helpers
// ---------------------------------------------------------------------------

/// Build the canonical `hf://owner/repo/path` URL (`path` may contain `/`).
fn hf_url(owner: &str, repo: &str, path: &str) -> String {
    format!("hf://{owner}/{repo}/{path}")
}

/// Parse a Hugging Face web URL of the form
/// `https://huggingface.co/{owner}/{repo}/resolve/{revision}/{path}` (or
/// `/blob/`), returning the `hf://` coordinate. Query/fragment are dropped.
fn parse_hf_web_url(raw: &str) -> Option<SourceCoord> {
    let rest = [
        "https://huggingface.co/",
        "http://huggingface.co/",
        "https://hf.co/",
        "http://hf.co/",
    ]
    .iter()
    .find_map(|p| raw.strip_prefix(p))?;

    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let parts: Vec<&str> = rest.split('/').collect();
    // owner / repo / (resolve|blob) / revision / path...
    if parts.len() < 5 || (parts[2] != "resolve" && parts[2] != "blob") {
        return None;
    }
    let (owner, repo, revision) = (parts[0], parts[1], parts[3]);
    let path = parts[4..].join("/");
    if owner.is_empty() || repo.is_empty() || revision.is_empty() || path.is_empty() {
        return None;
    }
    Some(SourceCoord {
        url: hf_url(owner, repo, &path),
        revision: revision.to_string(),
    })
}

/// Content keys for a set of coordinates, de-duplicated, order preserved.
fn keys_for_coords(coords: &[SourceCoord]) -> Vec<String> {
    let mut keys = Vec::new();
    for c in coords {
        let k = content_key(&c.url, &c.revision, None);
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys
}

// ---------------------------------------------------------------------------
// Offline mapper: Hugging Face hub cache
// ---------------------------------------------------------------------------

/// Decode a hub-cache repo folder `models--{owner}--{repo}` into
/// `(owner, repo)`. `/` in the repo id was encoded as `--`; owner namespaces
/// never contain `--`, so splitting at the first `--` after the prefix is safe.
fn decode_hub_repo_dir(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("models--")?;
    let (owner, repo) = rest.split_once("--")?;
    (!owner.is_empty() && !repo.is_empty()).then(|| (owner.to_string(), repo.to_string()))
}

/// Read `refs/*` as `(ref_name, commit)` pairs for a hub-cache repo dir.
fn read_hub_refs(repo_dir: &Path) -> Vec<(String, String)> {
    let refs_dir = repo_dir.join("refs");
    let mut out = Vec::new();
    walk_files(&refs_dir, |path, _| {
        let Ok(commit) = std::fs::read_to_string(path) else {
            return;
        };
        let commit = commit.trim().to_string();
        let Ok(rel) = path.strip_prefix(&refs_dir) else {
            return;
        };
        let name = rel.to_string_lossy().replace('\\', "/");
        if !commit.is_empty() && !name.is_empty() {
            out.push((name, commit));
        }
    });
    out
}

fn scan_hf_hub_cache(root: &Path, cfg: &ScanConfig, out: &mut Vec<DiscoveredSeed>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let repo_dir = entry.path();
        let Some((owner, repo)) = repo_dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(decode_hub_repo_dir)
        else {
            continue;
        };

        let refs = read_hub_refs(&repo_dir);
        let snapshots = repo_dir.join("snapshots");
        let Ok(snap_entries) = std::fs::read_dir(&snapshots) else {
            continue;
        };
        for snap in snap_entries.flatten() {
            let snap_dir = snap.path();
            let Some(commit) = snap_dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if !snap_dir.is_dir() {
                continue;
            }

            // Revisions this snapshot answers to: the commit itself, plus any
            // ref (e.g. `main`, tags) that points at it.
            let mut revisions: Vec<String> = vec![commit.clone()];
            revisions.extend(
                refs.iter()
                    .filter(|(_, c)| c == &commit)
                    .map(|(name, _)| name.clone()),
            );

            walk_files(&snap_dir, |path, size| {
                if !is_model_file(path, size, cfg) {
                    return;
                }
                let Ok(rel) = path.strip_prefix(&snap_dir) else {
                    return;
                };
                let rel = rel.to_string_lossy().replace('\\', "/");
                let url = hf_url(&owner, &repo, &rel);
                let coords: Vec<SourceCoord> = revisions
                    .iter()
                    .map(|r| SourceCoord {
                        url: url.clone(),
                        revision: r.clone(),
                    })
                    .collect();
                let keys = keys_for_coords(&coords);
                if !keys.is_empty() {
                    out.push(DiscoveredSeed {
                        content_keys: keys,
                        file_path: path.to_path_buf(),
                        source: "hf-hub-cache",
                    });
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Offline mapper: LM Studio (`<owner>/<repo>/<file>`)
// ---------------------------------------------------------------------------

fn scan_lmstudio(root: &Path, cfg: &ScanConfig, out: &mut Vec<DiscoveredSeed>) {
    walk_files(root, |path, size| {
        if !is_model_file(path, size, cfg) {
            return;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            return;
        };
        let comps: Vec<String> = rel
            .components()
            .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
            .collect();
        // Need at least owner / repo / file.
        if comps.len() < 3 {
            return;
        }
        let url = hf_url(&comps[0], &comps[1], &comps[2..].join("/"));
        out.push(DiscoveredSeed {
            content_keys: vec![content_key(&url, "main", None)],
            file_path: path.to_path_buf(),
            source: "lmstudio",
        });
    });
}

// ---------------------------------------------------------------------------
// Offline mapper: legacy llama.cpp cache (`.json` sidecar records the url)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LlamaCppSidecar {
    #[serde(default)]
    url: Option<String>,
}

/// Read the `<model>.json` (or `<stem>.json`) sidecar llama.cpp writes next to
/// a cached file and recover its Hugging Face coordinate from the `url` field.
fn read_llamacpp_sidecar(model: &Path) -> Option<SourceCoord> {
    let mut candidates = Vec::new();
    let mut full = model.as_os_str().to_owned();
    full.push(".json");
    candidates.push(PathBuf::from(full));
    candidates.push(model.with_extension("json"));

    for json in candidates {
        let Ok(bytes) = std::fs::read(&json) else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice::<LlamaCppSidecar>(&bytes) else {
            continue;
        };
        if let Some(coord) = meta.url.as_deref().and_then(parse_hf_web_url) {
            return Some(coord);
        }
    }
    None
}

fn scan_llamacpp_legacy(root: &Path, cfg: &ScanConfig, out: &mut Vec<DiscoveredSeed>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        if !md.is_file() || !is_model_file(&path, md.len(), cfg) {
            continue;
        }
        let Some(coord) = read_llamacpp_sidecar(&path) else {
            continue;
        };
        out.push(DiscoveredSeed {
            content_keys: vec![content_key(&coord.url, &coord.revision, None)],
            file_path: path,
            source: "llama.cpp",
        });
    }
}

// ---------------------------------------------------------------------------
// Offline mapper: Jan (`model.json` records `sources[].url`)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct JanModelJson {
    #[serde(default)]
    sources: Vec<JanSource>,
}

#[derive(Deserialize)]
struct JanSource {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    filename: Option<String>,
}

/// Locate the weight file a Jan `source` refers to inside its model directory.
fn jan_local_file(dir: &Path, src: &JanSource, cfg: &ScanConfig) -> Option<PathBuf> {
    if let Some(name) = &src.filename {
        let p = dir.join(name);
        if let Ok(md) = std::fs::metadata(&p) {
            if md.is_file() && is_model_file(&p, md.len(), cfg) {
                return Some(p);
            }
        }
    }
    // Fall back to the first model-format file in the directory.
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(md) = std::fs::metadata(&p) else {
            continue;
        };
        if md.is_file() && is_model_file(&p, md.len(), cfg) {
            return Some(p);
        }
    }
    None
}

fn scan_jan(root: &Path, cfg: &ScanConfig, out: &mut Vec<DiscoveredSeed>) {
    walk_files(root, |path, _| {
        if path.file_name().and_then(|n| n.to_str()) != Some("model.json") {
            return;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Ok(meta) = serde_json::from_slice::<JanModelJson>(&bytes) else {
            return;
        };
        let Some(dir) = path.parent() else {
            return;
        };
        for src in &meta.sources {
            let Some(coord) = src.url.as_deref().and_then(parse_hf_web_url) else {
                continue;
            };
            let Some(file_path) = jan_local_file(dir, src, cfg) else {
                continue;
            };
            out.push(DiscoveredSeed {
                content_keys: vec![content_key(&coord.url, &coord.revision, None)],
                file_path,
                source: "jan",
            });
        }
    });
}

// ---------------------------------------------------------------------------
// Discovery entry points
// ---------------------------------------------------------------------------

/// Merge seeds that resolved to the same file, unioning their content keys and
/// preserving first-seen order.
fn dedup_seeds(seeds: Vec<DiscoveredSeed>) -> Vec<DiscoveredSeed> {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut by_path: BTreeMap<PathBuf, DiscoveredSeed> = BTreeMap::new();
    for s in seeds {
        match by_path.get_mut(&s.file_path) {
            Some(existing) => {
                for k in s.content_keys {
                    if !existing.content_keys.contains(&k) {
                        existing.content_keys.push(k);
                    }
                }
            }
            None => {
                order.push(s.file_path.clone());
                by_path.insert(s.file_path.clone(), s);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|p| by_path.remove(&p))
        .collect()
}

/// Discover pre-downloaded models from the offline (exact-mapping) engine
/// locations: the Hugging Face hub cache, LM Studio, the legacy llama.cpp cache
/// and Jan. Pure and network-free — safe to call from tests and off-reactor.
pub fn discover_seeds(cfg: &ScanConfig) -> Vec<DiscoveredSeed> {
    let mut out = Vec::new();
    for root in hf_hub_cache_dirs() {
        scan_hf_hub_cache(&root, cfg, &mut out);
    }
    for root in lmstudio_dirs() {
        scan_lmstudio(&root, cfg, &mut out);
    }
    for root in llamacpp_legacy_dirs() {
        scan_llamacpp_legacy(&root, cfg, &mut out);
    }
    for root in jan_dirs() {
        scan_jan(&root, cfg, &mut out);
    }
    dedup_seeds(out)
}

/// Write a seed manifest per (discovered file, content key) into `registry_dir`
/// where the seed service picks it up. Discovered manifests are refreshed on
/// every scan (overwriting their own prior TTL); a `dfget`-authored manifest
/// for the same content key is only ever refreshed to point at a byte-identical
/// local copy, so overwriting is safe. Returns the number of manifests written.
fn register_discovered(registry_dir: &Path, cfg: &ScanConfig, seeds: &[DiscoveredSeed]) -> usize {
    let mut n = 0;
    for seed in seeds {
        for key in &seed.content_keys {
            match register_seed(
                registry_dir,
                &cfg.tracker_url,
                key,
                &seed.file_path,
                cfg.ttl,
            ) {
                Ok(()) => n += 1,
                Err(e) => tracing::warn!(
                    "failed to register discovered seed {}: {e}",
                    &key[..8.min(key.len())]
                ),
            }
        }
    }
    n
}

/// Long-lived scan service hosted by dfdaemon. On a fixed interval it walks the
/// configured engine directories, maps each pre-downloaded model back to its
/// Hugging Face content key(s) and writes seed manifests into `registry_dir`
/// (served + announced by the seed service). When `cfg.resolve` is set it also
/// runs the network name-search resolver for content-addressed stores (Ollama)
/// and loose files. Returns when `shutdown` resolves.
pub async fn run_scan_service(
    registry_dir: PathBuf,
    cfg: ScanConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&registry_dir)?;

    let mut tick = tokio::time::interval(SCAN_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Remembers resolver outcomes across scans so settled files never re-hit
    // the network.
    let mut resolve_cache = resolver::ResolveCache::new();

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tick.tick() => {
                // Directory walking is blocking I/O; keep it off the reactor.
                let scan_cfg = cfg.clone();
                let seeds = tokio::task::spawn_blocking(move || discover_seeds(&scan_cfg))
                    .await
                    .unwrap_or_default();
                let n = register_discovered(&registry_dir, &cfg, &seeds);
                if n > 0 {
                    tracing::info!("gguf scan: registered {n} seed(s) from local model dirs");
                }

                if cfg.resolve {
                    match resolver::resolve_all(&cfg, &mut resolve_cache).await {
                        Ok(resolved) => {
                            let n = register_discovered(&registry_dir, &cfg, &resolved);
                            if n > 0 {
                                tracing::info!("gguf scan: registered {n} seed(s) via name lookup");
                            }
                        }
                        Err(e) => tracing::warn!("gguf scan name lookup failed: {e}"),
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Network resolver (Ollama + loose files). Implemented in a later step.
// ---------------------------------------------------------------------------

mod resolver {
    use super::{
        dedup_paths, home_dir, is_model_file, path_from_env, walk_files, DiscoveredSeed, ScanConfig,
    };
    use crate::content_key;
    use serde::Deserialize;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// Cache of resolution outcomes keyed by a content identity (an Ollama blob
    /// sha256, or path+size for a loose file). `Some(keys)` is a confirmed
    /// match; `None` is a confirmed miss we won't retry. Kept across scans by
    /// the scan service so we don't re-hit the network for settled files.
    pub(super) type ResolveCache = HashMap<String, Option<Vec<String>>>;

    /// Per-scan cap on distinct Hugging Face repos queried, so a large loose
    /// collection can't hammer the API in one pass.
    const MAX_REPO_LOOKUPS: usize = 64;

    /// One local file to resolve to a Hugging Face coordinate.
    struct Candidate {
        /// Identity used for caching (blob sha256, or path+size).
        identity: String,
        /// Local file that will be seeded on a confirmed match.
        file_path: PathBuf,
        /// The file's sha256 when it is free to know (Ollama names blobs by it);
        /// otherwise `None` and we compute it lazily, only once a repo is found.
        known_sha256: Option<String>,
        /// A directly-known source repo (Ollama `hf.co` pulls record it), else
        /// `None` and we search by `search_name`.
        repo: Option<(String, String)>,
        /// Name to search Hugging Face with when `repo` is `None`.
        search_name: Option<String>,
        /// Origin, for logging.
        source: &'static str,
    }

    /// Forward name lookup: for content-addressed stores (Ollama) and loose
    /// files, find the source repo on Hugging Face — directly when the manifest
    /// records it, otherwise by searching for the model name — then confirm by
    /// comparing the repo file's sha256 to the local file. Seeds only on an
    /// exact hash match; anything not found or not matching is skipped.
    pub(super) async fn resolve_all(
        cfg: &ScanConfig,
        cache: &mut ResolveCache,
    ) -> anyhow::Result<Vec<DiscoveredSeed>> {
        let client = HfClient::new(cfg.hf_token.clone());
        let mut out = Vec::new();
        let mut budget = MAX_REPO_LOOKUPS;

        // Ollama first: the manifest names the source repo, so it is a direct
        // lookup rather than a fuzzy search — the strongest, cheapest case.
        let mut candidates = ollama_candidates(cfg);
        candidates.extend(loose_candidates(cfg));

        for cand in candidates {
            resolve_candidate(&client, cache, &mut out, &mut budget, cand).await;
        }
        Ok(out)
    }

    async fn resolve_candidate(
        client: &HfClient,
        cache: &mut ResolveCache,
        out: &mut Vec<DiscoveredSeed>,
        budget: &mut usize,
        cand: Candidate,
    ) {
        // Settled already? Reuse the outcome without touching the network.
        if let Some(entry) = cache.get(&cand.identity) {
            if let Some(keys) = entry {
                out.push(DiscoveredSeed {
                    content_keys: keys.clone(),
                    file_path: cand.file_path,
                    source: cand.source,
                });
            }
            return;
        }

        // Candidate repos: direct (Ollama) or via a name search (loose files).
        let repos: Vec<(String, String)> = match &cand.repo {
            Some(r) => vec![r.clone()],
            None => {
                let Some(name) = &cand.search_name else {
                    cache.insert(cand.identity, None);
                    return;
                };
                if *budget == 0 {
                    return; // out of budget this pass; retry on the next scan.
                }
                match client.search_repos(name, 5).await {
                    Ok(ids) => ids.iter().filter_map(|id| split_repo_id(id)).collect(),
                    Err(e) => {
                        tracing::debug!("HF search '{name}' failed: {e}");
                        return;
                    }
                }
            }
        };

        if repos.is_empty() {
            cache.insert(cand.identity, None);
            return;
        }

        // Local sha256: free for Ollama blobs, computed once for loose files.
        let local_sha = match &cand.known_sha256 {
            Some(s) => s.clone(),
            None => match compute_sha256(&cand.file_path).await {
                Some(s) => s,
                None => return,
            },
        };

        for (owner, repo) in repos {
            if *budget == 0 {
                return; // leave uncached so the next scan can finish the job.
            }
            *budget -= 1;
            let files = match client.repo_files(&owner, &repo).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::debug!("HF tree {owner}/{repo} failed: {e}");
                    continue;
                }
            };
            let hit = files.into_iter().find(|f| {
                f.lfs
                    .as_ref()
                    .is_some_and(|l| l.oid.eq_ignore_ascii_case(&local_sha))
            });
            if let Some(entry) = hit {
                let url = format!("hf://{owner}/{repo}/{}", entry.path);
                let keys = vec![content_key(&url, "main", None)];
                cache.insert(cand.identity.clone(), Some(keys.clone()));
                tracing::info!("resolved {} model to {} by name+hash", cand.source, url);
                out.push(DiscoveredSeed {
                    content_keys: keys,
                    file_path: cand.file_path,
                    source: cand.source,
                });
                return;
            }
        }

        // Searched/looked up but nothing matched by hash — remember the miss.
        cache.insert(cand.identity, None);
    }

    // -- Ollama -------------------------------------------------------------

    fn ollama_dirs() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(p) = path_from_env("OLLAMA_MODELS") {
            out.push(p);
        }
        if let Some(home) = home_dir() {
            out.push(home.join(".ollama").join("models"));
        }
        dedup_paths(out)
    }

    #[derive(Deserialize)]
    struct OllamaManifest {
        #[serde(default)]
        layers: Vec<OllamaLayer>,
    }

    #[derive(Deserialize)]
    struct OllamaLayer {
        #[serde(rename = "mediaType", default)]
        media_type: String,
        #[serde(default)]
        digest: String,
    }

    /// The sha256 (hex, no prefix) of the GGUF weight layer in an Ollama manifest.
    fn ollama_model_digest(manifest: &Path) -> Option<String> {
        let bytes = std::fs::read(manifest).ok()?;
        let m: OllamaManifest = serde_json::from_slice(&bytes).ok()?;
        m.layers
            .into_iter()
            .find(|l| l.media_type == "application/vnd.ollama.image.model")?
            .digest
            .strip_prefix("sha256:")
            .map(str::to_string)
    }

    /// Ollama manifests live at `manifests/<host>/<owner>/<repo>/<tag>`. Only
    /// `hf.co`/`huggingface.co` pulls carry a Hugging Face source; native
    /// `registry.ollama.ai` models are Ollama's own requantizations that do not
    /// exist on the Hub, so they are skipped.
    fn ollama_candidates(cfg: &ScanConfig) -> Vec<Candidate> {
        let mut out = Vec::new();
        for root in ollama_dirs() {
            let manifests = root.join("manifests");
            let blobs = root.join("blobs");
            walk_files(&manifests, |path, _| {
                let Ok(rel) = path.strip_prefix(&manifests) else {
                    return;
                };
                let comps: Vec<String> = rel
                    .components()
                    .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
                    .collect();
                // host / owner / repo / tag
                if comps.len() != 4 {
                    return;
                }
                if comps[0] != "hf.co" && comps[0] != "huggingface.co" {
                    return;
                }
                let Some(sha) = ollama_model_digest(path) else {
                    return;
                };
                let blob_path = blobs.join(format!("sha256-{sha}"));
                let Ok(md) = std::fs::metadata(&blob_path) else {
                    return;
                };
                if !md.is_file() || md.len() < cfg.min_size {
                    return;
                }
                out.push(Candidate {
                    identity: format!("ollama:{sha}"),
                    file_path: blob_path,
                    known_sha256: Some(sha),
                    repo: Some((comps[1].clone(), comps[2].clone())),
                    search_name: None,
                    source: "ollama",
                });
            });
        }
        out
    }

    // -- Loose files (GPT4All, oobabooga, user-specified dirs) --------------

    fn loose_dirs(cfg: &ScanConfig) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(home) = home_dir() {
            out.push(home.join(".cache").join("gpt4all"));
            out.push(
                home.join(".local")
                    .join("share")
                    .join("nomic.ai")
                    .join("GPT4All"),
            );
            out.push(home.join("text-generation-webui").join("models"));
        }
        out.extend(cfg.extra_dirs.iter().cloned());
        dedup_paths(out)
    }

    fn loose_candidates(cfg: &ScanConfig) -> Vec<Candidate> {
        let mut out = Vec::new();
        for root in loose_dirs(cfg) {
            walk_files(&root, |path, size| {
                if !is_model_file(path, size, cfg) {
                    return;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    return;
                };
                out.push(Candidate {
                    identity: format!("loose:{}:{size}", path.to_string_lossy()),
                    file_path: path.to_path_buf(),
                    known_sha256: None,
                    repo: None,
                    search_name: Some(search_name_from_filename(name)),
                    source: "loose",
                });
            });
        }
        out
    }

    /// Reduce a weight filename to a searchable model name by dropping the
    /// extension and a trailing quantization token (e.g.
    /// `Meta-Llama-3-8B-Instruct.Q4_K_M.gguf` -> `Meta-Llama-3-8B-Instruct`).
    fn search_name_from_filename(name: &str) -> String {
        let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
        match stem.rsplit_once('.') {
            Some((head, tail)) if looks_like_quant(tail) => head.to_string(),
            _ => stem.to_string(),
        }
    }

    fn looks_like_quant(s: &str) -> bool {
        let up = s.to_ascii_uppercase();
        up.starts_with('Q')
            || up.starts_with("IQ")
            || up.starts_with("F16")
            || up.starts_with("F32")
            || up.starts_with("BF16")
            || up.starts_with("FP16")
    }

    fn split_repo_id(id: &str) -> Option<(String, String)> {
        let (o, r) = id.split_once('/')?;
        (!o.is_empty() && !r.is_empty()).then(|| (o.to_string(), r.to_string()))
    }

    /// Stream the file through sha256 off the reactor.
    async fn compute_sha256(path: &Path) -> Option<String> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            use sha2::{Digest, Sha256};
            let mut file = std::fs::File::open(&path).ok()?;
            let mut hasher = Sha256::new();
            std::io::copy(&mut file, &mut hasher).ok()?;
            Some(hex::encode(hasher.finalize()))
        })
        .await
        .ok()
        .flatten()
    }

    // -- Hugging Face API ---------------------------------------------------

    #[derive(Deserialize)]
    struct HfTreeEntry {
        path: String,
        #[serde(default)]
        lfs: Option<HfLfs>,
    }

    #[derive(Deserialize)]
    struct HfLfs {
        /// sha256 of the file content for LFS objects.
        oid: String,
    }

    #[derive(Deserialize)]
    struct HfModelId {
        id: String,
    }

    struct HfClient {
        client: reqwest::Client,
        token: Option<String>,
    }

    impl HfClient {
        fn new(token: Option<String>) -> Self {
            Self {
                client: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .build()
                    .expect("reqwest client build"),
                token,
            }
        }

        fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
            match &self.token {
                Some(t) => req.bearer_auth(t),
                None => req,
            }
        }

        /// Every file in a repo (main branch) as tree entries, LFS oid included.
        async fn repo_files(&self, owner: &str, repo: &str) -> anyhow::Result<Vec<HfTreeEntry>> {
            let url = format!("https://huggingface.co/api/models/{owner}/{repo}/tree/main");
            let req = self
                .client
                .get(&url)
                .query(&[("recursive", "true"), ("expand", "true")]);
            let resp = self.authed(req).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("HF tree {owner}/{repo} returned {}", resp.status());
            }
            Ok(resp.json().await?)
        }

        /// Model repo ids matching `name`, most-relevant first.
        async fn search_repos(&self, name: &str, limit: usize) -> anyhow::Result<Vec<String>> {
            let req = self
                .client
                .get("https://huggingface.co/api/models")
                .query(&[("search", name), ("limit", &limit.to_string())]);
            let resp = self.authed(req).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("HF search returned {}", resp.status());
            }
            let models: Vec<HfModelId> = resp.json().await?;
            Ok(models.into_iter().map(|m| m.id).collect())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_search_name_strips_ext_and_quant() {
            assert_eq!(
                search_name_from_filename("Meta-Llama-3-8B-Instruct.Q4_K_M.gguf"),
                "Meta-Llama-3-8B-Instruct"
            );
            assert_eq!(
                search_name_from_filename("mistral-7b-v0.1.gguf"),
                "mistral-7b-v0.1"
            );
            assert_eq!(search_name_from_filename("model.safetensors"), "model");
        }

        #[test]
        fn test_split_repo_id() {
            assert_eq!(
                split_repo_id("TheBloke/Llama-2-7B-GGUF"),
                Some(("TheBloke".to_string(), "Llama-2-7B-GGUF".to_string()))
            );
            assert_eq!(split_repo_id("noslash"), None);
        }

        #[test]
        fn test_ollama_model_digest() {
            let dir = tempfile::tempdir().unwrap();
            let manifest = dir.path().join("manifest");
            std::fs::write(
                &manifest,
                br#"{"layers":[
                    {"mediaType":"application/vnd.ollama.image.template","digest":"sha256:aaaa"},
                    {"mediaType":"application/vnd.ollama.image.model","digest":"sha256:beef"}
                ]}"#,
            )
            .unwrap();
            assert_eq!(ollama_model_digest(&manifest).as_deref(), Some("beef"));
        }

        #[test]
        fn test_ollama_candidate_from_hf_manifest() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            // blobs/sha256-<hex>
            std::fs::create_dir_all(root.join("blobs")).unwrap();
            std::fs::write(root.join("blobs").join("sha256-beef"), b"GGUF-weights").unwrap();
            // manifests/hf.co/owner/repo/Q4_K_M
            let man_dir = root
                .join("manifests")
                .join("hf.co")
                .join("owner")
                .join("repo");
            std::fs::create_dir_all(&man_dir).unwrap();
            std::fs::write(
                man_dir.join("Q4_K_M"),
                br#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:beef"}]}"#,
            )
            .unwrap();

            // Exercise the manifest+blob mapping directly off the fixture root
            // (ollama_candidates itself resolves roots from env, which we avoid
            // mutating in tests).
            let mut out = Vec::new();
            let manifests = root.join("manifests");
            let blobs = root.join("blobs");
            walk_files(&manifests, |path, _| {
                let rel = path.strip_prefix(&manifests).unwrap();
                let comps: Vec<String> = rel
                    .components()
                    .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
                    .collect();
                if comps.len() == 4 && comps[0] == "hf.co" {
                    if let Some(sha) = ollama_model_digest(path) {
                        let blob = blobs.join(format!("sha256-{sha}"));
                        if blob.is_file() {
                            out.push((comps[1].clone(), comps[2].clone(), sha));
                        }
                    }
                }
            });
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].0, "owner");
            assert_eq!(out[0].1, "repo");
            assert_eq!(out[0].2, "beef");
        }

        #[test]
        fn test_native_ollama_manifest_skipped() {
            // registry.ollama.ai host must not produce a candidate.
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let man_dir = root
                .join("manifests")
                .join("registry.ollama.ai")
                .join("library")
                .join("llama3");
            std::fs::create_dir_all(&man_dir).unwrap();
            std::fs::write(man_dir.join("latest"), b"{}").unwrap();

            let manifests = root.join("manifests");
            let mut hosts = Vec::new();
            walk_files(&manifests, |path, _| {
                let rel = path.strip_prefix(&manifests).unwrap();
                if let Some(first) = rel.components().next() {
                    hosts.push(first.as_os_str().to_string_lossy().to_string());
                }
            });
            assert!(hosts.iter().all(|h| h != "hf.co" && h != "huggingface.co"));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> ScanConfig {
        ScanConfig {
            min_size: 0,
            resolve: false,
            hf_token: None,
            ..Default::default()
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn test_decode_hub_repo_dir() {
        assert_eq!(
            decode_hub_repo_dir("models--TheBloke--Llama-2-7B-GGUF"),
            Some(("TheBloke".to_string(), "Llama-2-7B-GGUF".to_string()))
        );
        // Repo name containing a `--` still splits at the first (the `/`).
        assert_eq!(
            decode_hub_repo_dir("models--org--a--b"),
            Some(("org".to_string(), "a--b".to_string()))
        );
        assert_eq!(decode_hub_repo_dir("datasets--org--repo"), None);
        assert_eq!(decode_hub_repo_dir("models--onlyone"), None);
    }

    #[test]
    fn test_parse_hf_web_url() {
        let c =
            parse_hf_web_url("https://huggingface.co/owner/repo/resolve/main/sub/x.gguf").unwrap();
        assert_eq!(c.url, "hf://owner/repo/sub/x.gguf");
        assert_eq!(c.revision, "main");

        // blob view + query string.
        let c =
            parse_hf_web_url("https://huggingface.co/o/r/blob/v1.0/m.gguf?download=true").unwrap();
        assert_eq!(c.url, "hf://o/r/m.gguf");
        assert_eq!(c.revision, "v1.0");

        assert!(parse_hf_web_url("https://example.com/o/r/resolve/main/x.gguf").is_none());
        assert!(parse_hf_web_url("https://huggingface.co/o/r/main/x.gguf").is_none());
    }

    #[test]
    fn test_hub_cache_maps_to_main_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let commit = "abc123def456";
        let repo = root.join("models--owner--repo");
        write(
            &repo.join("snapshots").join(commit).join("model.gguf"),
            b"GGUF-weights",
        );
        write(&repo.join("refs").join("main"), commit.as_bytes());

        let mut out = Vec::new();
        scan_hf_hub_cache(root, &test_cfg(), &mut out);
        assert_eq!(out.len(), 1, "one model file discovered");

        let seed = &out[0];
        let url = "hf://owner/repo/model.gguf";
        assert!(seed.content_keys.contains(&content_key(url, "main", None)));
        assert!(seed.content_keys.contains(&content_key(url, commit, None)));
    }

    #[test]
    fn test_hub_cache_preserves_subdir_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let commit = "c0ffee";
        let repo = root.join("models--org--Model");
        write(
            &repo
                .join("snapshots")
                .join(commit)
                .join("gguf")
                .join("q4.gguf"),
            b"GGUF-weights",
        );
        write(&repo.join("refs").join("main"), commit.as_bytes());

        let mut out = Vec::new();
        scan_hf_hub_cache(root, &test_cfg(), &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].content_keys.contains(&content_key(
            "hf://org/Model/gguf/q4.gguf",
            "main",
            None
        )));
    }

    #[test]
    fn test_lmstudio_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("owner").join("repo").join("m.Q4_K_M.gguf"),
            b"GGUF-weights",
        );

        let mut out = Vec::new();
        scan_lmstudio(root, &test_cfg(), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].content_keys,
            vec![content_key("hf://owner/repo/m.Q4_K_M.gguf", "main", None)]
        );
    }

    #[test]
    fn test_lmstudio_ignores_too_shallow() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Only one directory level: not owner/repo/file.
        write(&root.join("loose.gguf"), b"GGUF-weights");

        let mut out = Vec::new();
        scan_lmstudio(root, &test_cfg(), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn test_llamacpp_sidecar_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let model = root.join("owner_repo_model.gguf");
        write(&model, b"GGUF-weights");
        write(
            &root.join("owner_repo_model.gguf.json"),
            br#"{"url":"https://huggingface.co/owner/repo/resolve/main/model.gguf","etag":"x"}"#,
        );

        let mut out = Vec::new();
        scan_llamacpp_legacy(root, &test_cfg(), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].content_keys,
            vec![content_key("hf://owner/repo/model.gguf", "main", None)]
        );
    }

    #[test]
    fn test_llamacpp_without_sidecar_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("mystery.gguf"), b"GGUF-weights");

        let mut out = Vec::new();
        scan_llamacpp_legacy(root, &test_cfg(), &mut out);
        assert!(out.is_empty(), "no sidecar means no confident mapping");
    }

    #[test]
    fn test_jan_model_json_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let model_dir = root.join("my-model");
        write(&model_dir.join("model.gguf"), b"GGUF-weights");
        write(
            &model_dir.join("model.json"),
            br#"{"sources":[{"url":"https://huggingface.co/owner/repo/resolve/main/model.gguf","filename":"model.gguf"}]}"#,
        );

        let mut out = Vec::new();
        scan_jan(root, &test_cfg(), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file_path, model_dir.join("model.gguf"));
        assert_eq!(
            out[0].content_keys,
            vec![content_key("hf://owner/repo/model.gguf", "main", None)]
        );
    }

    #[test]
    fn test_min_size_and_format_filter() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let commit = "deadbeef";
        let repo = root.join("models--o--r");
        let snap = repo.join("snapshots").join(commit);
        write(&snap.join("model.gguf"), b"0123456789"); // 10 bytes
        write(&snap.join("config.json"), b"{}"); // wrong format
        write(&repo.join("refs").join("main"), commit.as_bytes());

        let cfg = ScanConfig {
            min_size: 100, // larger than the 10-byte model
            ..test_cfg()
        };
        let mut out = Vec::new();
        scan_hf_hub_cache(root, &cfg, &mut out);
        assert!(
            out.is_empty(),
            "min_size filters the model, format filters config.json"
        );

        let mut out = Vec::new();
        scan_hf_hub_cache(root, &test_cfg(), &mut out);
        assert_eq!(out.len(), 1, "only the .gguf, never config.json");
    }

    #[test]
    fn test_dedup_seeds_merges_keys() {
        let p = PathBuf::from("/models/x.gguf");
        let seeds = vec![
            DiscoveredSeed {
                content_keys: vec!["a".into(), "b".into()],
                file_path: p.clone(),
                source: "one",
            },
            DiscoveredSeed {
                content_keys: vec!["b".into(), "c".into()],
                file_path: p.clone(),
                source: "two",
            },
        ];
        let merged = dedup_seeds(seeds);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].content_keys, vec!["a", "b", "c"]);
    }
}
