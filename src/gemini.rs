//! Optional Gemini (gemini://) protocol server.
//!
//! Mirrors the HTTP reader at /, /feeds, /feed/{idx}, /groups, /group/{idx},
//! /item/{iid}. Admin endpoints, density toggles, and other "form" routes
//! are HTTP-only — Gemini has no request body, so they don't translate.
//!
//! ## Protocol notes
//!
//! Gemini is a single-shot request/response protocol over TLS:
//!
//! 1. Client opens a TLS connection (usually port 1965).
//! 2. Client sends `<URI><CR><LF>` (max 1024 bytes of URI).
//! 3. Server replies with `<two-digit status> <SP> <meta><CR><LF>[body]`.
//! 4. Server closes the connection.
//!
//! Server certificates are self-signed by convention — Gemini clients
//! pin them on first encounter ("Trust On First Use"). We generate a
//! cert at first launch with `rcgen` and persist it; on subsequent
//! launches we load it from disk so clients don't see a pin mismatch.
//!
//! ## HTML → gemtext
//!
//! Gemtext is line-oriented and has no inline links. We walk the
//! readability-extracted article body with `scraper`, emit headings
//! and paragraphs as text, and hoist every link to its own `=> URL
//! text` line directly after the paragraph that referenced it.

use std::{
    fmt::Write as _,
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use scraper::Html;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};
use tokio_rustls::{
    rustls::{pki_types::PrivateKeyDer, ServerConfig},
    TlsAcceptor,
};
use url::Url;

use crate::{
    config::GeminiConfig,
    extract::{extract_url, sanitize_images, BLOCKED_MARKER},
    feeds::{collect_entries, ensure_feeds, entry_full_html, item_id, EntryView},
    state::AppState,
    view::{now_secs, PAGE_SIZE},
};

const GEMTEXT_MIME: &str = "text/gemini; charset=utf-8";
const MAX_REQUEST_LINE: usize = 1024 + 2; // 1024 URI chars + CRLF
const REQUEST_TIMEOUT_S: u64 = 10;

pub async fn serve(state: Arc<AppState>, cfg: GeminiConfig) -> Result<()> {
    let acceptor = build_acceptor(&cfg)?;
    let listener = TcpListener::bind(&cfg.bind)
        .await
        .with_context(|| format!("binding gemini listener on {}", cfg.bind))?;
    tracing::info!("gemini listening on gemini://{}/", cfg.bind);
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("gemini: accept failed: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_one(state, acceptor, stream).await {
                tracing::debug!("gemini conn from {peer} ended: {e}");
            }
        });
    }
}

fn build_acceptor(cfg: &GeminiConfig) -> Result<TlsAcceptor> {
    let (cert_pem, key_pem) = load_or_generate_pem(
        &cfg.cert_pem,
        &cfg.key_pem,
        &cfg.hostnames,
    )?;
    let certs = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parsing gemini cert PEM")?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .context("reading gemini key PEM")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", cfg.key_pem.display()))?;

    // Force the ring crypto provider — picking ring explicitly makes
    // the build reproducible across machines where multiple providers
    // might be installed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, PrivateKeyDer::from(key))
        .context("loading gemini cert+key into rustls")?;
    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

/// Returns `(cert_pem, key_pem)`. If both files exist on disk we use
/// them as-is. Otherwise we generate a fresh self-signed pair, write
/// both files, and return them. Subsequent launches will then load.
fn load_or_generate_pem(cert: &Path, key: &Path, hostnames: &[String]) -> Result<(String, String)> {
    if cert.exists() && key.exists() {
        let c = std::fs::read_to_string(cert)
            .with_context(|| format!("reading {}", cert.display()))?;
        let k = std::fs::read_to_string(key)
            .with_context(|| format!("reading {}", key.display()))?;
        return Ok((c, k));
    }
    tracing::info!(
        "gemini: generating self-signed cert for {:?} → {} / {}",
        hostnames,
        cert.display(),
        key.display(),
    );
    let names = if hostnames.is_empty() {
        vec!["localhost".to_string()]
    } else {
        hostnames.to_vec()
    };
    let ck = rcgen::generate_simple_self_signed(names)
        .context("generating self-signed cert")?;
    let cert_pem = ck.cert.pem();
    let key_pem = ck.key_pair.serialize_pem();
    if let Some(p) = cert.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    std::fs::write(cert, &cert_pem).with_context(|| format!("writing {}", cert.display()))?;
    std::fs::write(key, &key_pem).with_context(|| format!("writing {}", key.display()))?;
    Ok((cert_pem, key_pem))
}

async fn handle_one(
    state: Arc<AppState>,
    acceptor: TlsAcceptor,
    stream: tokio::net::TcpStream,
) -> Result<()> {
    let mut tls = acceptor.accept(stream).await?;

    // Read the request line. Gemini caps the URI at 1024 bytes; we
    // read up to that + CRLF and bail on anything bigger or anything
    // without a CRLF within the timeout. No buffering layer needed —
    // it's one short line.
    let mut buf = [0u8; MAX_REQUEST_LINE];
    let mut filled = 0usize;
    let line = timeout(Duration::from_secs(REQUEST_TIMEOUT_S), async {
        loop {
            if filled >= buf.len() {
                return Err::<&str, anyhow::Error>(anyhow::anyhow!("request too long"));
            }
            let n = tls.read(&mut buf[filled..]).await?;
            if n == 0 {
                return Err(anyhow::anyhow!("EOF before CRLF"));
            }
            filled += n;
            if let Some(end) = find_crlf(&buf[..filled]) {
                let s = std::str::from_utf8(&buf[..end])
                    .map_err(|e| anyhow::anyhow!("non-utf-8 URI: {e}"))?;
                // SAFETY: extending lifetime to the outer block; buf
                // lives for the rest of the function.
                return Ok(unsafe { std::mem::transmute::<&str, &str>(s) });
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("request timed out"))??;

    let uri = match Url::parse(line) {
        Ok(u) => u,
        Err(_) => {
            return write_response(&mut tls, 59, "Bad request").await;
        }
    };

    let response = route(state, &uri).await;
    write_response(&mut tls, response.status, &response.body).await?;
    let _ = tls.shutdown().await;
    Ok(())
}

fn find_crlf(b: &[u8]) -> Option<usize> {
    for i in 1..b.len() {
        if b[i - 1] == b'\r' && b[i] == b'\n' {
            return Some(i - 1);
        }
    }
    None
}

/// Single write: `<status> <meta>\r\n` followed by an optional body.
async fn write_response<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    status: u16,
    body_or_meta: &str,
) -> Result<()> {
    let header = if (20..30).contains(&status) {
        // 20-29 are success; meta is the MIME type; body follows.
        format!("{} {}\r\n", status, GEMTEXT_MIME)
    } else {
        // For non-success responses, the meta IS the human message.
        format!("{} {}\r\n", status, body_or_meta)
    };
    w.write_all(header.as_bytes()).await?;
    if (20..30).contains(&status) {
        w.write_all(body_or_meta.as_bytes()).await?;
    }
    w.flush().await?;
    Ok(())
}

struct GemResponse {
    status: u16,
    body: String,
}
impl GemResponse {
    fn ok(body: String) -> Self {
        Self { status: 20, body }
    }
    fn not_found() -> Self {
        Self { status: 51, body: "Not found".into() }
    }
}

async fn route(state: Arc<AppState>, uri: &Url) -> GemResponse {
    let path = uri.path();
    let page_num = uri
        .query_pairs()
        .find(|(k, _)| k == "page")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(1);

    if path == "/" || path.is_empty() {
        return render_all_stories(state, page_num).await;
    }
    if path == "/feeds" {
        return render_feeds(state).await;
    }
    if let Some(rest) = path.strip_prefix("/feed/") {
        if let Ok(idx) = rest.parse::<usize>() {
            return render_one_feed(state, idx, page_num).await;
        }
    }
    if path == "/groups" {
        return render_groups(state).await;
    }
    if let Some(rest) = path.strip_prefix("/group/") {
        if let Ok(idx) = rest.parse::<usize>() {
            return render_one_group(state, idx, page_num).await;
        }
    }
    if let Some(iid) = path.strip_prefix("/item/") {
        return render_one_item(state, iid).await;
    }
    GemResponse::not_found()
}

// ---------------------------------------------------------------------------
// Page renderers

fn page_header(title: &str) -> String {
    let mut h = String::new();
    writeln!(h, "# {}", title).unwrap();
    h.push('\n');
    writeln!(h, "=> / All stories").unwrap();
    writeln!(h, "=> /feeds Feeds").unwrap();
    writeln!(h, "=> /groups Groups").unwrap();
    h.push('\n');
    h
}

fn render_listing(
    title: &str,
    entries: &[EntryView],
    page_num: usize,
    base_path: &str,
    show_source: bool,
) -> String {
    let total = entries.len();
    let total_pages = total.div_ceil(PAGE_SIZE).max(1);
    let page_num = page_num.max(1).min(total_pages);
    let start = (page_num - 1) * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(total);

    let mut out = page_header(title);
    if entries.is_empty() {
        out.push_str("No items.\n");
        return out;
    }
    for e in &entries[start..end] {
        let source = if show_source {
            format!(" — {}", e.feed_title)
        } else {
            String::new()
        };
        writeln!(
            out,
            "=> /item/{} {} — {}{}",
            e.iid, e.title, e.host, source
        )
        .unwrap();
    }
    if total_pages > 1 {
        out.push('\n');
        if page_num > 1 {
            writeln!(
                out,
                "=> {}?page={} Previous page",
                base_path,
                page_num - 1
            )
            .unwrap();
        }
        if page_num < total_pages {
            writeln!(
                out,
                "=> {}?page={} Next page",
                base_path,
                page_num + 1
            )
            .unwrap();
        }
        writeln!(out, "Page {} of {}", page_num, total_pages).unwrap();
    }
    out
}

async fn render_all_stories(state: Arc<AppState>, page_num: usize) -> GemResponse {
    let feeds = state.feeds.read().await.clone();
    let all_idxs: Vec<usize> = (0..feeds.len()).collect();
    ensure_feeds(state.clone(), &all_idxs, false).await;
    let cache = state.feed_cache.read().await;
    let entries = collect_entries(&feeds, &cache, &all_idxs);
    GemResponse::ok(render_listing("All stories", &entries, page_num, "/", true))
}

async fn render_feeds(state: Arc<AppState>) -> GemResponse {
    let feeds = state.feeds.read().await.clone();
    let all_idxs: Vec<usize> = (0..feeds.len()).collect();
    ensure_feeds(state.clone(), &all_idxs, false).await;
    let titles = state.feed_titles.read().await;
    let mut out = page_header("Feeds");
    if feeds.is_empty() {
        out.push_str("No feeds configured.\n");
        return GemResponse::ok(out);
    }
    for (i, url) in feeds.iter().enumerate() {
        let title = titles
            .get(i)
            .and_then(|t| t.as_deref())
            .unwrap_or(url.as_str());
        writeln!(out, "=> /feed/{} {}", i, title).unwrap();
    }
    GemResponse::ok(out)
}

async fn render_one_feed(state: Arc<AppState>, idx: usize, page_num: usize) -> GemResponse {
    let feeds = state.feeds.read().await.clone();
    if idx >= feeds.len() {
        return GemResponse::not_found();
    }
    ensure_feeds(state.clone(), &[idx], false).await;
    let cache = state.feed_cache.read().await;
    let title = cache
        .get(&idx)
        .and_then(|c| c.parsed.title.as_ref().map(|t| t.content.clone()))
        .unwrap_or_else(|| feeds[idx].clone());
    let entries = collect_entries(&feeds, &cache, &[idx]);
    GemResponse::ok(render_listing(
        &title,
        &entries,
        page_num,
        &format!("/feed/{}", idx),
        false,
    ))
}

async fn render_groups(state: Arc<AppState>) -> GemResponse {
    let groups = state.groups.read().await.clone();
    let mut out = page_header("Groups");
    if groups.is_empty() {
        out.push_str("No groups configured.\n");
        return GemResponse::ok(out);
    }
    for (i, g) in groups.iter().enumerate() {
        let count = g.feed_indices.len();
        writeln!(
            out,
            "=> /group/{} {} ({} feed{})",
            i,
            g.name,
            count,
            if count == 1 { "" } else { "s" }
        )
        .unwrap();
    }
    GemResponse::ok(out)
}

async fn render_one_group(state: Arc<AppState>, idx: usize, page_num: usize) -> GemResponse {
    let (name, indices) = {
        let groups = state.groups.read().await;
        let Some(g) = groups.get(idx) else {
            return GemResponse::not_found();
        };
        (g.name.clone(), g.feed_indices.clone())
    };
    let feeds = state.feeds.read().await.clone();
    ensure_feeds(state.clone(), &indices, false).await;
    let cache = state.feed_cache.read().await;
    let entries = collect_entries(&feeds, &cache, &indices);
    GemResponse::ok(render_listing(
        &name,
        &entries,
        page_num,
        &format!("/group/{}", idx),
        true,
    ))
}

async fn render_one_item(state: Arc<AppState>, iid: &str) -> GemResponse {
    // Try cache first.
    let cached = {
        let conn = state.db.lock().await;
        conn.query_row(
            "SELECT url, title, html FROM article WHERE id=?1",
            [iid],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .ok()
    };

    let (link, title, body_html) = if let Some(c) = cached {
        c
    } else {
        let n_feeds = state.feeds.read().await.len();
        let all_idxs: Vec<usize> = (0..n_feeds).collect();
        ensure_feeds(state.clone(), &all_idxs, false).await;
        let found = {
            let cache = state.feed_cache.read().await;
            let mut found: Option<(String, String, Option<String>)> = None;
            'outer: for &i in &all_idxs {
                let Some(cf) = cache.get(&i) else { continue };
                for e in &cf.parsed.entries {
                    let l = e
                        .links
                        .first()
                        .map(|l| l.href.clone())
                        .unwrap_or_default();
                    if l.is_empty() {
                        continue;
                    }
                    if item_id(&l) == iid {
                        let t = e
                            .title
                            .as_ref()
                            .map(|t| t.content.clone())
                            .unwrap_or_else(|| l.clone());
                        let full = entry_full_html(e);
                        found = Some((l, t, full));
                        break 'outer;
                    }
                }
            }
            found
        };
        let Some((link, title, full)) = found else {
            return GemResponse::not_found();
        };
        let extracted = if let Some(h) = full {
            sanitize_images(&h)
        } else {
            extract_url(&state.http, &link).await
        };
        if !extracted.contains(BLOCKED_MARKER) {
            let conn = state.db.lock().await;
            let _ = conn.execute(
                "INSERT OR REPLACE INTO article (id,url,title,html,fetched_at) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![iid, &link, &title, &extracted, now_secs()],
            );
        }
        (link, title, extracted)
    };

    let mut out = page_header(&title);
    writeln!(out, "=> {} Source: {}", link, hostname_of(&link)).unwrap();
    out.push('\n');
    out.push_str(&html_to_gemtext(&body_html));
    out.push('\n');
    GemResponse::ok(out)
}

fn hostname_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// HTML → gemtext
//
// Gemini has no inline links, no inline images. We walk top-level
// block elements and emit:
//
//   <h1>  → `# heading`
//   <h2>  → `## heading`
//   <h3>  → `### heading`
//   <p>   → paragraph text. Any inline `<a href>` is collected and
//          emitted as `=> url anchor-text` directly after the paragraph.
//          `<img>` becomes `=> src alt` (or `=> src image` if no alt).
//   <blockquote> → each text line prefixed with `> `.
//   <ul><li> → each item as `* item-text` (with link extraction).
//   <pre>  → ``` fenced block.
//
// Everything else is flattened to its text content.

fn html_to_gemtext(html: &str) -> String {
    let doc = Html::parse_fragment(html);
    let root = doc.root_element();
    let mut out = String::new();
    walk_block(root, &mut out);
    // Collapse 3+ consecutive newlines to two.
    let mut squashed = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank <= 1 {
                squashed.push('\n');
            }
        } else {
            blank = 0;
            squashed.push_str(line);
            squashed.push('\n');
        }
    }
    squashed
}

fn walk_block(node: scraper::ElementRef, out: &mut String) {
    for child in node.children() {
        if let Some(el) = scraper::ElementRef::wrap(child) {
            emit_element(el, out);
        } else if let Some(text) = child.value().as_text() {
            let t = text.trim();
            if !t.is_empty() {
                out.push_str(t);
                out.push('\n');
            }
        }
    }
}

fn emit_element(el: scraper::ElementRef, out: &mut String) {
    let name = el.value().name();
    match name {
        "h1" => {
            writeln!(out, "# {}", inline_text(el)).unwrap();
            out.push('\n');
        }
        "h2" => {
            writeln!(out, "## {}", inline_text(el)).unwrap();
            out.push('\n');
        }
        "h3" | "h4" | "h5" | "h6" => {
            writeln!(out, "### {}", inline_text(el)).unwrap();
            out.push('\n');
        }
        "p" | "section" | "article" | "div" => {
            let (text, links) = inline_text_and_links(el);
            if !text.trim().is_empty() {
                out.push_str(text.trim());
                out.push('\n');
            }
            for (url, anchor) in links {
                writeln!(out, "=> {} {}", url, anchor).unwrap();
            }
            if !text.trim().is_empty() || !el.children().next().is_none() {
                out.push('\n');
            }
        }
        "blockquote" => {
            for line in inline_text(el).lines() {
                let l = line.trim();
                if !l.is_empty() {
                    writeln!(out, "> {}", l).unwrap();
                }
            }
            out.push('\n');
        }
        "ul" | "ol" => {
            for li in el.children().filter_map(scraper::ElementRef::wrap) {
                if li.value().name() == "li" {
                    let (text, links) = inline_text_and_links(li);
                    writeln!(out, "* {}", text.trim()).unwrap();
                    for (url, anchor) in links {
                        writeln!(out, "=> {} {}", url, anchor).unwrap();
                    }
                }
            }
            out.push('\n');
        }
        "pre" => {
            out.push_str("```\n");
            out.push_str(&inline_text(el));
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n\n");
        }
        "img" => {
            let src = el.value().attr("src").unwrap_or("");
            if !src.is_empty() {
                let alt = el.value().attr("alt").unwrap_or("image");
                let alt = if alt.is_empty() { "image" } else { alt };
                writeln!(out, "=> {} {}", src, alt).unwrap();
            }
        }
        "a" => {
            // Bare <a> at block level (rare from readability) — emit
            // as a standalone link.
            if let Some(href) = el.value().attr("href") {
                let text = inline_text(el);
                let text = if text.trim().is_empty() { href } else { text.trim() };
                writeln!(out, "=> {} {}", href, text).unwrap();
            }
        }
        _ => {
            // Unknown block: descend.
            walk_block(el, out);
        }
    }
}

fn inline_text(el: scraper::ElementRef) -> String {
    let mut out = String::new();
    for t in el.text() {
        out.push_str(t);
    }
    out
}

/// Inline text plus a list of (href, anchor) pairs hoisted out of any
/// nested `<a>` and `<img>` elements.
fn inline_text_and_links(el: scraper::ElementRef) -> (String, Vec<(String, String)>) {
    let mut text = String::new();
    let mut links: Vec<(String, String)> = Vec::new();
    walk_inline(el, &mut text, &mut links);
    (text, links)
}

fn walk_inline(
    el: scraper::ElementRef,
    text: &mut String,
    links: &mut Vec<(String, String)>,
) {
    for child in el.children() {
        if let Some(child_el) = scraper::ElementRef::wrap(child) {
            match child_el.value().name() {
                "a" => {
                    let anchor = inline_text(child_el);
                    text.push_str(&anchor);
                    if let Some(href) = child_el.value().attr("href") {
                        let trimmed = anchor.trim().to_string();
                        let label = if trimmed.is_empty() {
                            href.to_string()
                        } else {
                            trimmed
                        };
                        links.push((href.to_string(), label));
                    }
                }
                "img" => {
                    let src = child_el.value().attr("src").unwrap_or("");
                    let alt = child_el.value().attr("alt").unwrap_or("image");
                    if !src.is_empty() {
                        let label = if alt.is_empty() { "image" } else { alt };
                        links.push((src.to_string(), label.to_string()));
                    }
                }
                "br" => text.push('\n'),
                _ => walk_inline(child_el, text, links),
            }
        } else if let Some(t) = child.value().as_text() {
            text.push_str(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_gemtext_renders_basic_structure() {
        let html = r#"<h1>Title</h1><p>First paragraph.</p><p>Second with <a href="https://example.com">a link</a> inside.</p>"#;
        let gem = html_to_gemtext(html);
        assert!(gem.contains("# Title"));
        assert!(gem.contains("First paragraph."));
        // Inline link text is preserved in the paragraph.
        assert!(gem.contains("Second with a link inside."));
        // And the link is hoisted to its own => line.
        assert!(gem.contains("=> https://example.com a link"));
    }

    #[test]
    fn html_to_gemtext_handles_lists() {
        let html = "<ul><li>one</li><li>two</li></ul>";
        let gem = html_to_gemtext(html);
        assert!(gem.contains("* one"));
        assert!(gem.contains("* two"));
    }

    #[test]
    fn html_to_gemtext_renders_blockquote() {
        let html = "<blockquote>quoted line</blockquote>";
        let gem = html_to_gemtext(html);
        assert!(gem.contains("> quoted line"));
    }

    #[test]
    fn html_to_gemtext_hoists_images() {
        let html = r#"<p>See <img src="https://x.com/p.jpg" alt="a barn"></p>"#;
        let gem = html_to_gemtext(html);
        assert!(gem.contains("=> https://x.com/p.jpg a barn"));
    }

    #[test]
    fn html_to_gemtext_handles_pre() {
        let html = "<pre>let x = 1;</pre>";
        let gem = html_to_gemtext(html);
        assert!(gem.contains("```"));
        assert!(gem.contains("let x = 1;"));
    }

    #[test]
    fn page_header_includes_navigation() {
        let h = page_header("X");
        assert!(h.starts_with("# X"));
        assert!(h.contains("=> / All stories"));
        assert!(h.contains("=> /feeds Feeds"));
        assert!(h.contains("=> /groups Groups"));
    }

    #[test]
    fn render_listing_paginates_with_prev_next_links() {
        let entries: Vec<_> = (0..PAGE_SIZE * 2 + 3)
            .map(|i| EntryView {
                iid: format!("{:016x}", i),
                title: format!("Story {}", i),
                host: "example.com".into(),
                published_ts: i as i64,
                feed_title: "F".into(),
            })
            .collect();
        let g1 = render_listing("All", &entries, 1, "/", true);
        let g2 = render_listing("All", &entries, 2, "/", true);
        let g3 = render_listing("All", &entries, 3, "/", true);

        assert!(g1.contains("=> /?page=2 Next page"));
        assert!(!g1.contains("Previous page"));

        assert!(g2.contains("=> /?page=1 Previous page"));
        assert!(g2.contains("=> /?page=3 Next page"));

        assert!(g3.contains("=> /?page=2 Previous page"));
        assert!(!g3.contains("Next page"));
    }

    #[test]
    fn render_listing_empty_state() {
        let g = render_listing("Empty", &[], 1, "/", false);
        assert!(g.contains("No items."));
    }

    #[test]
    fn find_crlf_locates_terminator() {
        assert_eq!(find_crlf(b"hi\r\n"), Some(2));
        assert_eq!(find_crlf(b"\r\n"), Some(0));
        assert_eq!(find_crlf(b"no terminator"), None);
        assert_eq!(find_crlf(b"\r"), None);
    }
}
