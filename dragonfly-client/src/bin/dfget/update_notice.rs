/*
 *     Copyright 2023 The Dragonfly Authors
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

//! Best-effort update / announcement notice for dfget.
//!
//! On a successful run, dfget fetches a small JSON file published on the project's
//! default branch and, if it advertises a newer version or carries an announcement
//! (e.g. a freshly shipped feature), prints it to stderr. Editing `notice.json` lets
//! the project broadcast a message to every user *without* cutting a new release.
//!
//! This is deliberately unobtrusive and can never break a download:
//!   * the network fetch has a short timeout and all errors are swallowed,
//!   * the result is cached locally so at most one fetch happens per `CACHE_TTL`,
//!   * the notice is printed only on the success path, to stderr, after the download,
//!   * it can be disabled with `--no-update-notice` or `DRAGONFLY_NO_UPDATE_NOTICE=1`.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::debug;

/// URL of the published notice file. Edit `notice.json` on the default branch to
/// broadcast a message to all users without cutting a new release.
const NOTICE_URL: &str =
    "https://raw.githubusercontent.com/JustDory/dragonfly-gguf-client/main/notice.json";

/// Version of the running binary (the crate version this was built from).
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a fetched notice is trusted before we hit the network again.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Maximum time we are willing to wait for the notice fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Environment variable that disables the update check entirely (any non-empty value).
const DISABLE_ENV: &str = "DRAGONFLY_NO_UPDATE_NOTICE";

/// Notice is the schema of the remote `notice.json`. Every field is optional so the
/// project can publish a version bump, a free-form announcement, or both.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Notice {
    /// Latest released version, e.g. "1.4.0". If newer than the running binary, an
    /// upgrade hint is shown.
    #[serde(default)]
    latest_version: Option<String>,

    /// Free-form announcement shown verbatim (e.g. "v1.4 adds NAT-traversal P2P").
    #[serde(default)]
    message: Option<String>,

    /// Where users should go to learn more or upgrade.
    #[serde(default)]
    url: Option<String>,
}

/// CachedNotice wraps a fetched notice together with the time it was fetched so we
/// can honour `CACHE_TTL` between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedNotice {
    fetched_at_secs: u64,
    notice: Notice,
}

/// Prints any pending update notice to stderr.
///
/// `disabled` mirrors the `--no-update-notice` flag; the `DRAGONFLY_NO_UPDATE_NOTICE`
/// environment variable is honoured as well. This never returns an error and never
/// panics — failures are logged at debug level and otherwise ignored.
pub async fn print_if_any(disabled: bool) {
    if disabled || env_disabled() {
        return;
    }

    // Serve from cache when it is still fresh; otherwise fetch and refresh it.
    let notice = match load_fresh_cache() {
        Some(notice) => notice,
        None => match fetch().await {
            Some(notice) => {
                store_cache(&notice);
                notice
            }
            None => return,
        },
    };

    let lines = collect_lines(&notice);
    if !lines.is_empty() {
        render(&lines);
    }
}

/// Returns true if the update check is disabled via the environment.
fn env_disabled() -> bool {
    std::env::var_os(DISABLE_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// Fetches and parses the remote notice, returning None on any error.
async fn fetch() -> Option<Notice> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("dfget/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| debug!("update notice: building client failed: {err}"))
        .ok()?;

    let resp = client
        .get(NOTICE_URL)
        .send()
        .await
        .map_err(|err| debug!("update notice: fetch failed: {err}"))
        .ok()?;

    if !resp.status().is_success() {
        debug!("update notice: unexpected status {}", resp.status());
        return None;
    }

    resp.json::<Notice>()
        .await
        .map_err(|err| debug!("update notice: parse failed: {err}"))
        .ok()
}

/// Builds the lines to display, if any, from a notice.
fn collect_lines(notice: &Notice) -> Vec<String> {
    let mut lines = Vec::new();

    if let Some(message) = notice.message.as_deref() {
        let message = message.trim();
        if !message.is_empty() {
            lines.extend(message.lines().map(str::to_string));
        }
    }

    if let Some(latest) = notice.latest_version.as_deref() {
        if is_newer(latest, CURRENT_VERSION) {
            lines.push(format!(
                "A new version of dfget is available: {CURRENT_VERSION} -> {latest}"
            ));
            if let Some(url) = notice.url.as_deref() {
                let url = url.trim();
                if !url.is_empty() {
                    lines.push(format!("Upgrade: {url}"));
                }
            }
        }
    }

    lines
}

/// Renders the notice as a boxed block on stderr, with a touch of colour on a TTY.
fn render(lines: &[String]) {
    let (hl, rs) = if std::io::stderr().is_terminal() {
        ("\x1b[1;33m", "\x1b[0m") // bold yellow
    } else {
        ("", "")
    };

    eprintln!();
    eprintln!("{hl}╭─ dragonfly-gguf notice ─────────────────────────────╮{rs}");
    for line in lines {
        eprintln!("{hl}│{rs} {line}");
    }
    eprintln!("{hl}╰─────────────────────────────────────────────────────╯{rs}");
}

/// Returns true if `remote` is strictly newer than `current`.
///
/// Compares dot-separated numeric components (a leading "v" is ignored and any
/// non-numeric suffix on a component, like "-rc1", is dropped). Missing trailing
/// components are treated as 0, so "1.4" and "1.4.0" compare equal.
fn is_newer(remote: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim()
            .trim_start_matches(['v', 'V'])
            .split('.')
            .map(|part| {
                let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<u64>().unwrap_or(0)
            })
            .collect()
    };

    let remote = parse(remote);
    let current = parse(current);
    let len = remote.len().max(current.len());
    for i in 0..len {
        let r = remote.get(i).copied().unwrap_or(0);
        let c = current.get(i).copied().unwrap_or(0);
        if r != c {
            return r > c;
        }
    }
    false
}

/// Path of the local notice cache file.
fn cache_path() -> PathBuf {
    let dir = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache").join("dragonfly"))
        .unwrap_or_else(std::env::temp_dir);
    dir.join("gguf-update-notice.json")
}

/// Loads the cached notice if it exists and is still within `CACHE_TTL`.
fn load_fresh_cache() -> Option<Notice> {
    let raw = std::fs::read(cache_path()).ok()?;
    let cached: CachedNotice = serde_json::from_slice(&raw).ok()?;
    let age = now_secs().saturating_sub(cached.fetched_at_secs);
    (age <= CACHE_TTL.as_secs()).then_some(cached.notice)
}

/// Persists a freshly fetched notice to the cache (best effort).
fn store_cache(notice: &Notice) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let cached = CachedNotice {
        fetched_at_secs: now_secs(),
        notice: notice.clone(),
    };

    if let Ok(bytes) = serde_json::to_vec(&cached) {
        let _ = std::fs::write(path, bytes);
    }
}

/// Current unix time in seconds (0 if the clock is before the epoch).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_basic() {
        assert!(is_newer("1.4.0", "1.3.12"));
        assert!(is_newer("2.0.0", "1.9.9"));
        assert!(is_newer("1.3.13", "1.3.12"));
        assert!(!is_newer("1.3.12", "1.3.12"));
        assert!(!is_newer("1.3.11", "1.3.12"));
        assert!(!is_newer("1.2.0", "1.3.0"));
    }

    #[test]
    fn test_is_newer_handles_prefixes_and_suffixes() {
        assert!(is_newer("v1.4.0", "1.3.0"));
        assert!(!is_newer("v1.3.0", "v1.3.0"));
        assert!(is_newer("1.4.0-rc1", "1.3.0"));
        // Differing component lengths: missing trailing parts are zero.
        assert!(!is_newer("1.4", "1.4.0"));
        assert!(is_newer("1.4.1", "1.4"));
    }

    #[test]
    fn test_is_newer_garbage_is_safe() {
        assert!(!is_newer("", "1.3.0"));
        assert!(!is_newer("not-a-version", "1.3.0"));
    }

    #[test]
    fn test_collect_lines_message_only() {
        let notice = Notice {
            latest_version: None,
            message: Some("  hello world  ".to_string()),
            url: None,
        };
        assert_eq!(collect_lines(&notice), vec!["hello world".to_string()]);
    }

    #[test]
    fn test_collect_lines_multiline_message() {
        let notice = Notice {
            message: Some("line one\nline two".to_string()),
            ..Default::default()
        };
        assert_eq!(
            collect_lines(&notice),
            vec!["line one".to_string(), "line two".to_string()]
        );
    }

    #[test]
    fn test_collect_lines_version_newer() {
        let notice = Notice {
            latest_version: Some("9.9.9".to_string()),
            message: None,
            url: Some("https://example.com".to_string()),
        };
        let lines = collect_lines(&notice);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("9.9.9"));
        assert!(lines[1].contains("https://example.com"));
    }

    #[test]
    fn test_collect_lines_version_not_newer_is_silent() {
        let notice = Notice {
            latest_version: Some("0.0.1".to_string()),
            message: None,
            url: Some("https://example.com".to_string()),
        };
        assert!(collect_lines(&notice).is_empty());
    }

    #[test]
    fn test_collect_lines_empty_notice_is_silent() {
        assert!(collect_lines(&Notice::default()).is_empty());
    }

    #[test]
    fn test_notice_deserializes_partial_json() {
        // Only a message field present — version/url default to None.
        let notice: Notice = serde_json::from_str(r#"{"message":"hi"}"#).unwrap();
        assert_eq!(notice.message.as_deref(), Some("hi"));
        assert!(notice.latest_version.is_none());
        assert!(notice.url.is_none());

        // Empty object is valid and yields an all-None notice.
        let empty: Notice = serde_json::from_str("{}").unwrap();
        assert!(collect_lines(&empty).is_empty());
    }
}
