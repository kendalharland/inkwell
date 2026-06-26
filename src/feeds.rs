//! Feed fetching, parsing, in-memory caching, and entry projection.

use std::{collections::HashMap, sync::Arc, time::SystemTime};

use anyhow::Result;
use feed_rs::{
    model::{Entry, Feed as ParsedFeed},
    parser,
};
use futures::future::join_all;
use sha1::{Digest, Sha1};
use url::Url;

use crate::state::{AppState, CachedFeed};

/// Stable, short identifier for a feed entry. Derived from the link URL so:
///
/// 1. Articles cached in SQLite stay reachable across server restarts and
///    feed-roll-off (the row survives even when the entry no longer appears
///    in the live feed).
/// 2. The same URL surfaced from multiple feeds resolves to one cache row.
///
/// 16 hex chars = 64 bits, ample for collision-free personal use; short
/// enough to keep URLs nice on the Kindle nav.
pub fn item_id(link: &str) -> String {
    let mut h = Sha1::new();
    h.update(link.as_bytes());
    let bytes = h.finalize();
    let hex = format!("{:x}", bytes);
    hex.chars().take(16).collect()
}

/// Strict shape check matching what [`item_id`] emits. Used by route
/// handlers that accept an iid in the URL path so an attacker can't
/// inject markup or shell-like sequences through the path parameter
/// (see #16).
pub fn is_valid_iid(s: &str) -> bool {
    s.len() == 16 && s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// One outbound HTTP + parse. No caching, no retry — caller decides.
pub async fn fetch_feed_once(http: &reqwest::Client, url: &str) -> Result<ParsedFeed> {
    let resp = http.get(url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    let parsed = parser::parse(bytes.as_ref())?;
    Ok(parsed)
}

/// Fetch every requested feed in parallel unless its in-memory cache is
/// younger than `state.feed_ttl`. With `force = true` the TTL check is
/// skipped — used by the background refresh job which must guarantee a
/// fresh snapshot regardless of how recently a user request warmed the
/// cache.
///
/// Failures per feed are logged and skipped — one broken feed doesn't
/// affect siblings. This means the cache may have stale entries for a
/// down feed; the user sees the last good snapshot.
pub async fn ensure_feeds(state: Arc<AppState>, indices: &[usize], force: bool) {
    // Snapshot the URL list once up front: handler-scoped lock holds
    // are cheap, but if the admin UI swaps the feeds out mid-call we
    // don't want to fetch a URL that just got removed.
    let feeds_snapshot: Vec<String> = state.feeds.read().await.clone();

    let now = SystemTime::now();
    let to_fetch: Vec<usize> = if force {
        indices.to_vec()
    } else {
        let cache = state.feed_cache.read().await;
        indices
            .iter()
            .copied()
            .filter(|i| {
                cache
                    .get(i)
                    .map(|c| {
                        now.duration_since(c.fetched_at)
                            .map(|d| d > state.feed_ttl)
                            .unwrap_or(true)
                    })
                    .unwrap_or(true)
            })
            .collect()
    };
    if to_fetch.is_empty() {
        return;
    }

    let jobs = to_fetch.into_iter().filter_map(|i| {
        let url = feeds_snapshot.get(i).cloned()?;
        let state = state.clone();
        Some(async move {
            match fetch_feed_once(&state.http, &url).await {
                Ok(parsed) => Some((i, parsed)),
                Err(e) => {
                    tracing::warn!("feed {} fetch failed: {}", url, e);
                    None
                }
            }
        })
    });
    let results = join_all(jobs).await;

    let mut cache = state.feed_cache.write().await;
    let mut titles = state.feed_titles.write().await;
    let fetched_at = SystemTime::now();
    for opt in results {
        if let Some((i, parsed)) = opt {
            // Titles vec may have been resized by the admin UI between
            // when we snapshot'd `feeds_snapshot` and now — guard the
            // index so we don't panic on a concurrent shrink.
            if i < titles.len() {
                if let Some(t) = parsed.title.as_ref().map(|t| t.content.clone()) {
                    titles[i] = Some(t);
                }
            }
            cache.insert(i, CachedFeed { parsed, fetched_at });
        }
    }
}

/// Listing-shaped view of a feed entry. We project once and then sort/page
/// by `published_ts` (descending) so the rendering layer doesn't reach back
/// into feed-rs types.
pub struct EntryView {
    pub iid: String,
    pub title: String,
    pub host: String,
    /// Original article URL. Carried alongside the iid so the bookmark
    /// form can persist it directly — the read-later table needs the
    /// real URL, not just the hashed id.
    pub url: String,
    /// Unix seconds. `0` means "no date in the feed" — these items fall to
    /// the bottom of the sort.
    pub published_ts: i64,
    pub feed_title: String,
}

pub fn collect_entries(
    feeds: &[String],
    cache: &HashMap<usize, CachedFeed>,
    indices: &[usize],
) -> Vec<EntryView> {
    let mut out = Vec::new();
    for &i in indices {
        let Some(cf) = cache.get(&i) else { continue };
        let fallback = feeds.get(i).cloned().unwrap_or_default();
        let feed_title = cf
            .parsed
            .title
            .as_ref()
            .map(|t| t.content.clone())
            .unwrap_or(fallback);
        for e in &cf.parsed.entries {
            let link = e
                .links
                .first()
                .map(|l| l.href.clone())
                .unwrap_or_default();
            if link.is_empty() {
                continue;
            }
            let title = e
                .title
                .as_ref()
                .map(|t| t.content.clone())
                .unwrap_or_else(|| link.clone());
            let host = Url::parse(&link)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_string()))
                .unwrap_or_default();
            let published_ts = e
                .published
                .or(e.updated)
                .map(|d| d.timestamp())
                .unwrap_or(0);
            out.push(EntryView {
                iid: item_id(&link),
                title,
                host,
                url: link,
                published_ts,
                feed_title: feed_title.clone(),
            });
        }
    }
    out.sort_by(|a, b| b.published_ts.cmp(&a.published_ts));
    out
}

/// Return the feed's own article body iff it looks like full text.
///
/// The 1500-char threshold filters out feeds that emit short blurbs in
/// `<content>` (lots of mainstream RSS does this for SEO). When the body
/// is short we fall through to live extraction so the reader still gets
/// the real article. A more sophisticated heuristic (HTML tag density,
/// paragraph count) wasn't worth the code; 1500 chars catches every false
/// "full-text" feed I've seen in practice.
pub fn entry_full_html(e: &Entry) -> Option<String> {
    if let Some(c) = &e.content {
        if let Some(body) = &c.body {
            if body.len() > 1500 {
                return Some(body.clone());
            }
        }
    }
    if let Some(s) = &e.summary {
        if s.content.len() > 1500 {
            return Some(s.content.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_id_is_deterministic() {
        assert_eq!(item_id("https://example.com/a"), item_id("https://example.com/a"));
    }

    #[test]
    fn item_id_changes_with_url() {
        assert_ne!(item_id("https://example.com/a"), item_id("https://example.com/b"));
    }

    #[test]
    fn item_id_is_16_hex_chars() {
        let id = item_id("https://example.com/article");
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn is_valid_iid_accepts_item_id_output() {
        // What item_id emits must always pass — otherwise a fresh feed
        // entry would be unbookmarkable.
        for link in [
            "https://example.com/a",
            "https://example.com/article?id=1",
            "http://x",
        ] {
            assert!(is_valid_iid(&item_id(link)));
        }
    }

    #[test]
    fn is_valid_iid_rejects_html_injection_and_other_shapes() {
        // The path param goes into HTML attributes; reject any shape
        // that could carry a quote, tag, or anything outside lowercase
        // hex.
        for bad in [
            "",
            "abc",                 // too short
            "abcdef0123456789a",   // too long
            "ABCDEF0123456789",    // uppercase
            "ghijklmnopqrstuv",    // non-hex chars
            "0123456789abcde\"",   // quote
            "0123456789abc<\"\">", // tag bait
            "../../../etc/passwd",
            "0123 456789abcdef",   // space
        ] {
            assert!(!is_valid_iid(bad), "{:?} should be rejected", bad);
        }
    }

    fn content_with_body(s: impl Into<String>) -> feed_rs::model::Content {
        let mut c = feed_rs::model::Content::default();
        c.body = Some(s.into());
        c
    }

    fn text_with_content(s: impl Into<String>) -> feed_rs::model::Text {
        feed_rs::model::Text {
            content_type: "text/html".parse::<mediatype::MediaTypeBuf>().unwrap(),
            src: None,
            content: s.into(),
        }
    }

    #[test]
    fn entry_full_html_returns_long_content() {
        let mut entry = Entry::default();
        entry.content = Some(content_with_body("x".repeat(1600)));
        assert_eq!(entry_full_html(&entry).map(|s| s.len()), Some(1600));
    }

    #[test]
    fn entry_full_html_skips_short_content() {
        let mut entry = Entry::default();
        entry.content = Some(content_with_body("x".repeat(800)));
        assert!(entry_full_html(&entry).is_none());
    }

    #[test]
    fn entry_full_html_falls_back_to_long_summary() {
        let mut entry = Entry::default();
        entry.summary = Some(text_with_content("y".repeat(1600)));
        assert_eq!(entry_full_html(&entry).map(|s| s.len()), Some(1600));
    }

    #[test]
    fn entry_full_html_none_when_empty() {
        let entry = Entry::default();
        assert!(entry_full_html(&entry).is_none());
    }

    // Unused now that collect_entries takes a `&[String]` instead of
    // &AppState, but kept here as a sanity check that constructing a
    // bare AppState still type-checks if a future test needs it.
    #[allow(dead_code)]
    fn fake_state() -> AppState {
        AppState {
            feeds: tokio::sync::RwLock::new(Vec::new()),
            feed_titles: tokio::sync::RwLock::new(Vec::new()),
            groups: tokio::sync::RwLock::new(Vec::new()),
            http: reqwest::Client::new(),
            discovery_http: reqwest::Client::new(),
            feed_cache: tokio::sync::RwLock::new(HashMap::new()),
            db: tokio::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
            feed_ttl: std::time::Duration::from_secs(60),
            article_ttl_secs: 86400,
            compact_default: false,
            dark_default: false,
            feed_search: crate::config::FeedSearchConfig::default(),
        }
    }

    fn rss_with(items: &[(&str, &str, &str)]) -> feed_rs::model::Feed {
        let mut body = String::from(r#"<?xml version="1.0"?><rss version="2.0"><channel><title>F1</title>"#);
        for (title, link, pub_iso) in items {
            body.push_str(&format!(
                "<item><title>{}</title><link>{}</link><pubDate>{}</pubDate></item>",
                title, link, pub_iso
            ));
        }
        body.push_str("</channel></rss>");
        feed_rs::parser::parse(body.as_bytes()).unwrap()
    }

    #[test]
    fn collect_entries_sorts_newest_first() {
        let feeds = vec!["https://example.com/feed".to_string()];
        let parsed = rss_with(&[
            ("Old", "https://example.com/a", "Mon, 01 Jan 2024 00:00:00 GMT"),
            ("New", "https://example.com/b", "Mon, 01 Jan 2026 00:00:00 GMT"),
            ("Mid", "https://example.com/c", "Mon, 01 Jan 2025 00:00:00 GMT"),
        ]);
        let mut cache = HashMap::new();
        cache.insert(0, CachedFeed { parsed, fetched_at: std::time::SystemTime::now() });
        let entries = collect_entries(&feeds, &cache, &[0]);
        let titles: Vec<&str> = entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["New", "Mid", "Old"]);
    }

    #[test]
    fn collect_entries_skips_empty_links() {
        let feeds = vec!["https://example.com/feed".to_string()];
        let body = r#"<?xml version="1.0"?>
            <rss version="2.0"><channel><title>F</title>
                <item><title>has link</title><link>https://example.com/x</link></item>
                <item><title>linkless</title></item>
            </channel></rss>"#;
        let parsed = feed_rs::parser::parse(body.as_bytes()).unwrap();
        let mut cache = HashMap::new();
        cache.insert(0, CachedFeed { parsed, fetched_at: std::time::SystemTime::now() });
        let entries = collect_entries(&feeds, &cache, &[0]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "has link");
    }

    #[test]
    fn collect_entries_pulls_host_from_link() {
        let feeds = vec!["https://example.com/feed".to_string()];
        let parsed = rss_with(&[
            ("a", "https://foo.example.com/path", "Mon, 01 Jan 2025 00:00:00 GMT"),
        ]);
        let mut cache = HashMap::new();
        cache.insert(0, CachedFeed { parsed, fetched_at: std::time::SystemTime::now() });
        let entries = collect_entries(&feeds, &cache, &[0]);
        assert_eq!(entries[0].host, "foo.example.com");
    }

    #[test]
    fn collect_entries_uses_feed_title_when_set() {
        let feeds = vec!["https://example.com/feed".to_string()];
        let parsed = rss_with(&[
            ("a", "https://example.com/x", "Mon, 01 Jan 2025 00:00:00 GMT"),
        ]);
        let mut cache = HashMap::new();
        cache.insert(0, CachedFeed { parsed, fetched_at: std::time::SystemTime::now() });
        let entries = collect_entries(&feeds, &cache, &[0]);
        assert_eq!(entries[0].feed_title, "F1");
    }
}
