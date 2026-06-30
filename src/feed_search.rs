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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{FeedSearchConfig, FeedSearchProvider};

/// Cap on how many redirects [`safe_fetch_text`] will follow before
/// giving up. Each hop is independently checked against
/// [`is_disallowed_ip`], so a public→loopback redirect chain is
/// rejected at the moment it tries to switch hosts.
const MAX_REDIRECTS: usize = 5;

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
///
/// SSRF defense (see #15): every hop's host is resolved and checked
/// against a private/loopback/link-local block-list before the request
/// is issued. Redirects are followed manually so a public→internal
/// redirect chain is rejected at the moment of the switch — the
/// underlying `http` client must be built with `Policy::none()` so it
/// does not auto-follow.
async fn link_autodiscovery(http: &reqwest::Client, query: &str) -> Result<Vec<SearchResult>> {
    let base = normalize_to_url(query);
    let (final_url, html) = safe_fetch_text(http, &base, MAX_REDIRECTS).await?;
    let doc = scraper::Html::parse_document(&html);
    let selector = scraper::Selector::parse(r#"link[rel="alternate"]"#)
        .map_err(|e| anyhow::anyhow!("selector parse: {e}"))?;
    let base_url = url::Url::parse(&final_url).ok();
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
        let title = attrs
            .attr("title")
            .map(|s| s.to_string())
            .unwrap_or_else(|| abs.clone());
        out.push(SearchResult {
            url: abs,
            title,
            source: "autodiscovery",
        });
    }
    Ok(out)
}

/// Issue a GET that:
/// 1. Refuses any URL whose scheme isn't `http`/`https`.
/// 2. Refuses any URL whose host (literal IP, or DNS A/AAAA records)
///    sits in a private/loopback/link-local/multicast range.
/// 3. Manually follows up to `max_hops` redirects, repeating (1) and
///    (2) on each hop's `Location` so a public 302 can't smuggle the
///    request into the internal network.
///
/// Returns the final URL (post-redirect) so the caller can resolve
/// relative HTML hrefs against it.
async fn safe_fetch_text(
    http: &reqwest::Client,
    initial: &str,
    max_hops: usize,
) -> Result<(String, String)> {
    let mut url = url::Url::parse(initial).context("parsing initial URL")?;
    for hop in 0..=max_hops {
        if !matches!(url.scheme(), "http" | "https") {
            anyhow::bail!("blocked: non-http(s) scheme {:?}", url.scheme());
        }
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("URL has no host"))?
            .to_string();
        check_host_is_public(&host).await?;
        let resp = http.get(url.as_str()).send().await?;
        if !resp.status().is_redirection() {
            let final_url = url.to_string();
            let text = resp.error_for_status()?.text().await?;
            return Ok((final_url, text));
        }
        if hop == max_hops {
            anyhow::bail!("too many redirects ({})", max_hops);
        }
        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("redirect without Location"))?;
        url = url.join(loc).context("joining redirect target")?;
    }
    unreachable!("loop bound is max_hops, body returns or bails on every iteration")
}

/// Resolve `host` and bail if any address sits in a range we never
/// want a server-side discovery fetch to reach. Handles both literal
/// IPs (no DNS) and hostnames (resolves via the system stub).
pub(crate) async fn check_host_is_public(host: &str) -> Result<()> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_disallowed_ip(ip) {
            anyhow::bail!("blocked private/internal IP {}", ip);
        }
        return Ok(());
    }
    let addrs = tokio::net::lookup_host((host, 80_u16))
        .await
        .with_context(|| format!("dns lookup of {}", host))?;
    let mut saw_any = false;
    for sa in addrs {
        saw_any = true;
        if is_disallowed_ip(sa.ip()) {
            anyhow::bail!("{} resolves to disallowed IP {}", host, sa.ip());
        }
    }
    if !saw_any {
        anyhow::bail!("{} resolved to no addresses", host);
    }
    Ok(())
}

fn is_disallowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_disallowed_ipv4(v4),
        IpAddr::V6(v6) => is_disallowed_ipv6(v6),
    }
}

fn is_disallowed_ipv4(v4: Ipv4Addr) -> bool {
    if v4.is_loopback() || v4.is_private() || v4.is_link_local() {
        return true;
    }
    if v4.is_broadcast() || v4.is_unspecified() || v4.is_multicast() {
        return true;
    }
    if v4.is_documentation() {
        return true;
    }
    // CGNAT 100.64.0.0/10 — `is_shared` is unstable in std.
    let o = v4.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

fn is_disallowed_ipv6(v6: Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
        return true;
    }
    let s = v6.segments();
    // fc00::/7 unique local
    if (s[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fe80::/10 link-local
    if (s[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // ::ffff:0:0/96 IPv4-mapped — check the embedded v4.
    if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0xffff {
        let o = v6.octets();
        return is_disallowed_ipv4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    false
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
            let abs = base
                .join(href)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| href.to_string());
            let title = a
                .attr("title")
                .map(|s| s.to_string())
                .unwrap_or_else(|| abs.clone());
            out.push(SearchResult {
                url: abs,
                title,
                source: "autodiscovery",
            });
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

    #[test]
    fn autodiscovery_skips_links_with_no_type_attribute() {
        // Some sites omit `type` on what is in practice an RSS feed.
        // Document the current behavior (skip) — if we ever flip to
        // accepting these, this test's assertion moves with the rule.
        let html = r##"<html><head>
            <link rel="alternate" href="/feed.xml" title="Site feed">
            </head></html>"##;
        let rs = parse_links(html);
        assert!(
            rs.is_empty(),
            "links without type should currently be skipped"
        );
    }

    #[test]
    fn autodiscovery_skips_links_with_wrong_rel_value() {
        // `rel="stylesheet"` etc. should never make it through — the
        // selector is `link[rel="alternate"]` and we depend on that.
        let html = r##"<html><head>
            <link rel="stylesheet" type="application/rss+xml" href="/styles.xml">
            <link rel="icon" type="application/rss+xml" href="/icon.xml">
            </head></html>"##;
        let rs = parse_links(html);
        assert!(rs.is_empty());
    }

    // ----- SSRF guards (#15) ------------------------------------------------

    fn v4(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }
    fn v6(s: &str) -> Ipv6Addr {
        s.parse().unwrap()
    }

    #[test]
    fn ipv4_filter_blocks_private_loopback_and_friends() {
        // Loopback, RFC1918, link-local, CGNAT, unspecified, broadcast,
        // multicast, documentation — all the ranges an attacker would
        // use to pivot inside the host's network.
        for s in [
            "127.0.0.1",
            "127.5.5.5",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "169.254.169.254", // AWS/GCP IMDS
            "100.64.0.1",      // CGNAT
            "100.127.255.255", // CGNAT
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",   // TEST-NET-1
            "203.0.113.1", // TEST-NET-3
        ] {
            assert!(is_disallowed_ipv4(v4(s)), "{} should be blocked", s);
        }
    }

    #[test]
    fn ipv4_filter_allows_public_addresses() {
        for s in [
            "1.1.1.1",
            "8.8.8.8",
            "151.101.0.81",
            "100.63.255.255",
            "100.128.0.0",
        ] {
            assert!(!is_disallowed_ipv4(v4(s)), "{} should be allowed", s);
        }
    }

    #[test]
    fn ipv6_filter_blocks_loopback_link_local_and_ula() {
        for s in [
            "::1",                    // loopback
            "::",                     // unspecified
            "fe80::1",                // link-local
            "fc00::1",                // ULA
            "fd12:3456:789a::1",      // ULA
            "ff00::1",                // multicast
            "::ffff:127.0.0.1",       // v4-mapped loopback
            "::ffff:10.0.0.5",        // v4-mapped private
            "::ffff:169.254.169.254", // v4-mapped IMDS
        ] {
            assert!(is_disallowed_ipv6(v6(s)), "{} should be blocked", s);
        }
    }

    #[test]
    fn ipv6_filter_allows_globally_routable_addresses() {
        for s in [
            "2001:4860:4860::8888", // Google DNS
            "2606:4700::1111",      // Cloudflare DNS
            "2400:cb00::",
        ] {
            assert!(!is_disallowed_ipv6(v6(s)), "{} should be allowed", s);
        }
    }

    #[tokio::test]
    async fn check_host_rejects_literal_loopback_without_dns() {
        assert!(check_host_is_public("127.0.0.1").await.is_err());
        assert!(check_host_is_public("169.254.169.254").await.is_err());
        assert!(check_host_is_public("::1").await.is_err());
    }
}
