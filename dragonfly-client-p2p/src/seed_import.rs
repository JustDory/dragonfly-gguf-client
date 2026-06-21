use std::path::{Path, PathBuf};

/// A `.gguf` file found in a local model cache that is eligible to be seeded.
#[derive(Debug, Clone)]
pub struct SeedCandidate {
    /// `gguf://owner/repo/filename.gguf` URL for this file.
    pub gguf_url: String,
    /// Content revision — the HuggingFace commit hash from the cache directory structure.
    pub revision: String,
    /// Absolute path to the `.gguf` file on disk (symlinks resolved).
    pub file_path: PathBuf,
}

/// Scan a HuggingFace model cache directory for `.gguf` files.
///
/// The HuggingFace CLI stores model snapshots at:
/// ```text
/// {cache_dir}/models--{owner}--{repo}/snapshots/{commit}/{filename}.gguf
/// ```
/// Files in snapshots are usually symlinks into a `blobs/` sibling directory;
/// this function resolves them so `file_path` always points at the real blob.
///
/// Every file found is returned as a [`SeedCandidate`] with the correct
/// `gguf://` URL and commit-based revision, so its P2P content key matches
/// any other peer that downloaded the same blob from HuggingFace.
pub fn scan_hf_cache(cache_dir: &Path) -> Vec<SeedCandidate> {
    let mut candidates = Vec::new();

    let Ok(top_entries) = std::fs::read_dir(cache_dir) else {
        return candidates;
    };

    for entry in top_entries.flatten() {
        let dir_name = entry.file_name();
        let dir_name = dir_name.to_string_lossy();

        let Some(rest) = dir_name.strip_prefix("models--") else {
            continue;
        };

        // HuggingFace encodes `owner/repo` as `owner--repo` (double-hyphen).
        // Owner and repo names cannot themselves contain `--`, so split_once is safe.
        let Some((owner, repo)) = rest.split_once("--") else {
            continue;
        };

        let snapshots_dir = entry.path().join("snapshots");
        let Ok(commits) = std::fs::read_dir(&snapshots_dir) else {
            continue;
        };

        for commit_entry in commits.flatten() {
            if !commit_entry.path().is_dir() {
                continue;
            }
            let revision = commit_entry.file_name().to_string_lossy().to_string();

            let Ok(files) = std::fs::read_dir(commit_entry.path()) else {
                continue;
            };

            for file_entry in files.flatten() {
                let file_name = file_entry.file_name();
                let file_name_str = file_name.to_string_lossy();
                if !file_name_str.ends_with(".gguf") {
                    continue;
                }

                // Resolve symlinks (snapshots/ → blobs/). Skip dangling links.
                let file_path = match file_entry.path().canonicalize() {
                    Ok(p) if p.is_file() => p,
                    _ => continue,
                };

                candidates.push(SeedCandidate {
                    gguf_url: format!("gguf://{owner}/{repo}/{file_name_str}"),
                    revision: revision.clone(),
                    file_path,
                });
            }
        }
    }

    candidates
}

/// Default HuggingFace cache directory, respecting environment overrides.
///
/// Resolution order:
/// 1. `$HF_HOME/hub`
/// 2. `$XDG_CACHE_HOME/huggingface/hub`
/// 3. `~/.cache/huggingface/hub`
pub fn default_hf_cache_dir() -> PathBuf {
    if let Some(hf_home) = std::env::var_os("HF_HOME") {
        return PathBuf::from(hf_home).join("hub");
    }
    let cache_base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache"))
        })
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    cache_base.join("huggingface").join("hub")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_hf_cache(
        base: &Path,
        owner: &str,
        repo: &str,
        commit: &str,
        files: &[&str],
    ) {
        let snap = base
            .join(format!("models--{owner}--{repo}"))
            .join("snapshots")
            .join(commit);
        fs::create_dir_all(&snap).unwrap();
        for f in files {
            fs::write(snap.join(f), b"fake gguf content").unwrap();
        }
    }

    #[test]
    fn scan_finds_gguf_files() {
        let dir = tempfile::tempdir().unwrap();
        make_hf_cache(
            dir.path(),
            "bartowski",
            "Qwen2-0.5B-Instruct-GGUF",
            "abc123def456",
            &["Qwen2-0.5B-Instruct-Q4_K_M.gguf", "Qwen2-0.5B-Instruct-Q8_0.gguf"],
        );
        make_hf_cache(
            dir.path(),
            "meta-llama",
            "Llama-3-8B-Instruct",
            "deadbeef0000",
            &["Meta-Llama-3-8B-Instruct-Q4_K_M.gguf"],
        );

        let candidates = scan_hf_cache(dir.path());
        assert_eq!(candidates.len(), 3);

        let urls: Vec<&str> = candidates.iter().map(|c| c.gguf_url.as_str()).collect();
        assert!(urls.iter().any(|u| u.starts_with("gguf://bartowski/Qwen2-0.5B-Instruct-GGUF/")));
        assert!(urls.iter().any(|u| u.starts_with("gguf://meta-llama/Llama-3-8B-Instruct/")));

        let revisions: Vec<&str> = candidates.iter().map(|c| c.revision.as_str()).collect();
        assert!(revisions.contains(&"abc123def456"));
        assert!(revisions.contains(&"deadbeef0000"));
    }

    #[test]
    fn scan_skips_non_gguf_files() {
        let dir = tempfile::tempdir().unwrap();
        let snap = dir
            .path()
            .join("models--owner--repo")
            .join("snapshots")
            .join("rev123");
        fs::create_dir_all(&snap).unwrap();
        fs::write(snap.join("config.json"), b"{}").unwrap();
        fs::write(snap.join("model.safetensors"), b"data").unwrap();
        fs::write(snap.join("model.gguf"), b"gguf").unwrap();

        let candidates = scan_hf_cache(dir.path());
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].gguf_url.ends_with("model.gguf"));
    }

    #[test]
    fn scan_empty_cache_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let candidates = scan_hf_cache(dir.path());
        assert!(candidates.is_empty());
    }

    #[test]
    fn scan_nonexistent_dir_returns_nothing() {
        let candidates = scan_hf_cache(Path::new("/nonexistent/path/that/does/not/exist"));
        assert!(candidates.is_empty());
    }
}
