//! Feed-discovery autocomplete for the admin UI.
//!
//! Given a user query (a domain or URL fragment), fan out to every
//! configured provider and return a flat list of candidate feed URLs.
//! Provider failures are swallowed — the autocomplete is a convenience,
//! not a correctness requirement, so a flaky third party shouldn't
//! make the admin page error out.
//!
//! Adding a provider: extend [`crate::config::FeedSearchProvider`],
//! add a fetcher fn here, match the new variant in
//! [`search_all`]. Results from each are tagged via the `source` field
//! so the UI could later show provenance if it wanted to.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{FeedSearchConfig, FeedSearchProvider};

#[derive(Debug, Serialize, Clone)]
pub struct SearchResult {
    pub url: String,
    pub title: String,
    pub source: &'static str,
}

pub async fn search_all(
    http: &reqwest::Client,
    cfg: &FeedSearchConfig,
    query: &str,
) -> Vec<SearchResult> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for p in &cfg.providers {
        let r = match p {
            FeedSearchProvider::LinkAutoDiscovery => link_autodiscovery(http, query).await,
            FeedSearchProvider::Feedsearch => feedsearch(http, query).await,
        };
        match r {
            Ok(rs) => out.extend(rs),
            Err(e) => tracing::warn!("feed_search: provider {:?} failed: {e:#}", p),
        }
    }
    // Dedup by URL while preserving order — a URL that two providers
    // both return appears only once, anchored to the first that yielded it.
    let mut seen = std::collections::HashSet::new();
    out.retain(|r| seen.insert(r.url.clone()));
    out
}

/// Fetch the user's target URL and surface every `<link rel="alternate">`
/// whose MIME type advertises a feed. Returns absolute URLs even when
/// the page used relative `href`s.
async fn link_autodiscovery(http: &reqwest::Client, query: &str) -> Result<Vec<SearchResult>> {
    let base = normalize_to_url(query);
    let html = http
        .get(&base)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let doc = scraper::Html::parse_document(&html);
    let selector = scraper::Selector::parse(r#"link[rel="alternate"]"#)
        .map_err(|e| anyhow::anyhow!("selector parse: {e}"))?;
    let base_url = url::Url::parse(&base).ok();
    let mut out = Vec::new();
    for el in doc.select(&selector) {
        let attrs = el.value();
        let href = match attrs.attr("href") {
            Some(h) => h,
            None => continue,
        };
        let mime = attrs.attr("type").unwrap_or("");
        if !is_feed_mime(mime) {
            continue;
        }
        let abs = base_url
            .as_ref()
            .and_then(|b| b.join(href).ok())
            .map(|u| u.to_string())
            .unwrap_or_else(|| href.to_string());
        let title = attrs.attr("title").map(|s| s.to_string()).unwrap_or_else(|| abs.clone());
        out.push(SearchResult {
            url: abs,
            title,
            source: "autodiscovery",
        });
    }
    Ok(out)
}

fn normalize_to_url(q: &str) -> String {
    if q.starts_with("http://") || q.starts_with("https://") {
        q.to_string()
    } else {
        format!("https://{}", q.trim_start_matches('/'))
    }
}

fn is_feed_mime(mime: &str) -> bool {
    // The spec'd set; covers RSS, Atom, and JSON Feed plus the
    // generic `application/xml` and `text/xml` that some sites use
    // when the link points at a feed.
    matches!(
        mime.split(';').next().unwrap_or("").trim(),
        "application/rss+xml"
            | "application/atom+xml"
            | "application/feed+json"
            | "application/json"
            | "application/xml"
            | "text/xml"
    )
}

async fn feedsearch(http: &reqwest::Client, query: &str) -> Result<Vec<SearchResult>> {
    #[derive(Deserialize)]
    struct Item {
        url: String,
        #[serde(default)]
        title: Option<String>,
    }
    let items: Vec<Item> = http
        .get("https://feedsearch.dev/api/v1/search")
        .query(&[("url", query)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(items
        .into_iter()
        .map(|i| SearchResult {
            title: i.title.unwrap_or_else(|| i.url.clone()),
            url: i.url,
            source: "feedsearch",
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_query_short_circuits_without_calling_providers() {
        let cfg = FeedSearchConfig::default();
        let http = reqwest::Client::new();
        let out = search_all(&http, &cfg, "   ").await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn empty_provider_list_yields_no_results() {
        let cfg = FeedSearchConfig { providers: vec![] };
        let http = reqwest::Client::new();
        let out = search_all(&http, &cfg, "lobste.rs").await;
        assert!(out.is_empty());
    }

    #[test]
    fn normalize_to_url_handles_bare_domains_and_full_urls() {
        assert_eq!(normalize_to_url("lobste.rs"), "https://lobste.rs");
        assert_eq!(normalize_to_url("https://lobste.rs"), "https://lobste.rs");
        assert_eq!(normalize_to_url("http://example.com"), "http://example.com");
        assert_eq!(normalize_to_url("/lobste.rs"), "https://lobste.rs");
    }

    #[test]
    fn is_feed_mime_accepts_rss_atom_jsonfeed_and_generic_xml() {
        assert!(is_feed_mime("application/rss+xml"));
        assert!(is_feed_mime("application/atom+xml"));
        assert!(is_feed_mime("application/feed+json"));
        assert!(is_feed_mime("application/xml"));
        assert!(is_feed_mime("text/xml"));
        // Charset trailers — strip and still recognize.
        assert!(is_feed_mime("application/rss+xml; charset=UTF-8"));
        // Negatives.
        assert!(!is_feed_mime("text/html"));
        assert!(!is_feed_mime(""));
    }

    fn parse_links(html: &str) -> Vec<SearchResult> {
        // Mirrors the parsing path of link_autodiscovery without
        // touching the network — same selector, same MIME filter, same
        // relative-resolution against a synthetic base URL.
        let base = url::Url::parse("https://example.com/").unwrap();
        let doc = scraper::Html::parse_document(html);
        let sel = scraper::Selector::parse(r#"link[rel="alternate"]"#).unwrap();
        let mut out = Vec::new();
        for el in doc.select(&sel) {
            let a = el.value();
            let href = match a.attr("href") {
                Some(h) => h,
                None => continue,
            };
            if !is_feed_mime(a.attr("type").unwrap_or("")) {
                continue;
            }
            let abs = base.join(href).map(|u| u.to_string()).unwrap_or_else(|_| href.to_string());
            let title = a.attr("title").map(|s| s.to_string()).unwrap_or_else(|| abs.clone());
            out.push(SearchResult { url: abs, title, source: "autodiscovery" });
        }
        out
    }

    #[test]
    fn autodiscovery_finds_rss_and_atom_links_and_resolves_relative_hrefs() {
        let html = r##"<html><head>
            <link rel="alternate" type="application/rss+xml" href="/feed.xml" title="Main feed">
            <link rel="alternate" type="application/atom+xml" href="https://other.example/atom.xml" title="Other">
            <link rel="alternate" type="text/html" href="/print">
            <link rel="stylesheet" href="/x.css">
            </head></html>"##;
        let rs = parse_links(html);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://example.com/feed.xml");
        assert_eq!(rs[0].title, "Main feed");
        assert_eq!(rs[1].url, "https://other.example/atom.xml");
    }

    #[test]
    fn autodiscovery_skips_links_without_href_or_with_non_feed_mime() {
        let html = r##"<html><head>
            <link rel="alternate" type="application/rss+xml">
            <link rel="alternate" type="application/json+activitystreams" href="/as">
            </head></html>"##;
        let rs = parse_links(html);
        assert!(rs.is_empty());
    }
}
