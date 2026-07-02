//! Embedded registry UI: `GET /` renders the live `/contents` listing as a
//! server-side HTML table (format badge, name, size, seeds, age), with a
//! search box (`?q=`) and a format/category dropdown (`?format=`).
//!
//! Everything a peer announced (filename, format) is untrusted input and is
//! HTML-escaped at render time — announce validation only guarantees length
//! caps and no control characters, not markup safety.

use crate::store::{ContentSummary, PeerStore};
use std::time::{SystemTime, UNIX_EPOCH};

const STYLE: &str = r#"
body { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
       margin: 2rem auto; max-width: 72rem; padding: 0 1rem; color: #1f2328; }
h1 { font-size: 1.3rem; }
form { margin: 1rem 0; display: flex; gap: .5rem; flex-wrap: wrap; }
input, select, button { font: inherit; padding: .3rem .5rem; }
table { border-collapse: collapse; width: 100%; }
th, td { border: 1px solid #d0d7de; padding: .45rem .6rem; text-align: left; }
th { background: #f6f8fa; }
td.num { text-align: right; }
.badge { display: inline-block; padding: .1rem .45rem; border-radius: .4rem;
         font-size: .75rem; font-weight: 700; background: #eaeef2; color: #57606a; }
.badge-gguf { background: #ddf4ff; color: #0969da; }
.badge-safetensors { background: #dafbe1; color: #1a7f37; }
.badge-bin, .badge-pt, .badge-onnx { background: #fff8c5; color: #9a6700; }
.seeds { color: #1a7f37; font-weight: 700; }
.key { color: #57606a; font-size: .75rem; }
.empty { color: #57606a; padding: 2rem 0; }
footer { margin-top: 1rem; color: #57606a; font-size: .8rem; }
"#;

/// Minimal HTML escaping for untrusted text rendered into element content or
/// double-quoted attribute values.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Humanizes a byte count ("1.6 GB", "512 MB", "740 B").
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Renders a unix timestamp as a coarse age ("3 d ago", "5 min ago").
fn age(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

fn badge(format: Option<&str>) -> String {
    match format {
        Some(f) => {
            let class = if matches!(f, "gguf" | "safetensors" | "bin" | "pt" | "onnx") {
                format!("badge badge-{f}")
            } else {
                "badge".to_string()
            };
            format!(
                r#"<span class="{class}">{}</span>"#,
                escape(&f.to_uppercase())
            )
        }
        None => r#"<span class="badge">?</span>"#.to_string(),
    }
}

fn row(now: u64, c: &ContentSummary) -> String {
    let name = c.filename.as_deref().unwrap_or("(unnamed)");
    let size = c
        .size
        .map(human_size)
        .unwrap_or_else(|| "\u{2014}".to_string());
    let short_key = &c.content_key[..16.min(c.content_key.len())];
    format!(
        "<tr><td>{}</td>\
         <td>{}<br><span class=\"key\" title=\"{}\">{}\u{2026}</span></td>\
         <td class=\"num\">{}</td>\
         <td class=\"num seeds\">{}</td>\
         <td>{}</td></tr>",
        badge(c.format.as_deref()),
        escape(name),
        escape(&c.content_key),
        escape(short_key),
        escape(&size),
        c.providers,
        age(now, c.first_seen),
    )
}

/// Renders the full registry page. `format`/`q` mirror the `/contents` query
/// parameters and pre-fill the filter controls.
pub fn page(store: &PeerStore, format: Option<&str>, q: Option<&str>, limit: usize) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Facet options come from the unfiltered listing so a filter never hides
    // the other categories from the dropdown.
    let all = store.list_contents(None, None, limit);
    let mut formats: Vec<String> = all
        .iter()
        .filter_map(|c| c.format.as_ref().map(|f| f.to_ascii_lowercase()))
        .collect();
    formats.sort();
    formats.dedup();

    let rows: Vec<ContentSummary> = store.list_contents(format, q, limit);

    let options: String = formats
        .iter()
        .map(|f| {
            let selected = if Some(f.as_str()) == format.map(|s| s.to_ascii_lowercase()).as_deref()
            {
                " selected"
            } else {
                ""
            };
            format!(
                r#"<option value="{0}"{selected}>{1}</option>"#,
                escape(f),
                escape(&f.to_uppercase())
            )
        })
        .collect();

    let body = if rows.is_empty() {
        r#"<p class="empty">Nothing is seeded (or matches the filter) right now. Pull a file with dfget and it will appear here.</p>"#.to_string()
    } else {
        let rows_html: String = rows.iter().map(|c| row(now, c)).collect();
        format!(
            "<table><thead><tr>\
             <th>Type</th><th>Name (order by: seeds \u{2193})</th><th>Size</th>\
             <th>Seeds</th><th>First seen</th>\
             </tr></thead><tbody>{rows_html}</tbody></table>"
        )
    };

    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Dragonfly GGUF tracker registry</title>
<style>{STYLE}</style></head>
<body>
<h1>&#128009; Dragonfly P2P registry</h1>
<form method="get" action="/">
  <input type="search" name="q" placeholder="search filename&#8230;" value="{q}">
  <select name="format">
    <option value="">all types</option>
    {options}
  </select>
  <button type="submit">Filter</button>
</form>
{body}
<footer>{count} listed &middot; live view of what the swarm is seeding &middot;
API: <code>GET /contents?format=&amp;q=&amp;limit=</code></footer>
</body></html>"#,
        q = escape(q.unwrap_or("")),
        count = rows.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ContentMeta;

    #[test]
    fn escapes_untrusted_filenames() {
        let store = PeerStore::new(1800, 100);
        store.announce(
            "a".repeat(64),
            "n1".into(),
            "{}".into(),
            Some(ContentMeta {
                filename: Some("<script>alert(1)</script>.gguf".into()),
                format: Some("gguf".into()),
                size: Some(1),
            }),
        );
        let html = page(&store, None, None, 100);
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;.gguf"));
    }

    #[test]
    fn renders_rows_and_filters() {
        let store = PeerStore::new(1800, 100);
        store.announce(
            "a".repeat(64),
            "n1".into(),
            "{}".into(),
            Some(ContentMeta {
                filename: Some("model.safetensors".into()),
                format: Some("safetensors".into()),
                size: Some(1_717_986_918), // ~1.6 GB
            }),
        );
        let html = page(&store, None, None, 100);
        assert!(html.contains("model.safetensors"));
        assert!(html.contains("SAFETENSORS"));
        assert!(html.contains("1.6 GB"));

        // A format filter that matches nothing renders the empty state but
        // still offers the existing formats in the dropdown.
        let filtered = page(&store, Some("gguf"), None, 100);
        assert!(filtered.contains("Nothing is seeded"));
        assert!(filtered.contains(r#"<option value="safetensors">"#));
    }

    #[test]
    fn human_sizes_and_ages() {
        assert_eq!(human_size(740), "740 B");
        assert_eq!(human_size(512 * 1024 * 1024), "512.0 MB");
        assert_eq!(human_size(43_180_465_192), "40.2 GB");
        assert_eq!(age(1000, 1000), "just now");
        assert_eq!(age(1000 + 300, 1000), "5 min ago");
        assert_eq!(age(1000 + 7200, 1000), "2 h ago");
        assert_eq!(age(1000 + 3 * 86_400, 1000), "3 d ago");
    }
}
