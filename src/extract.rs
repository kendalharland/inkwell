//! Live article fetch + readability extraction with a graceful blocked-site
//! fallback. Also post-processes images so the Kindle browser gets either a
//! renderable image or a text fallback — never a broken-image icon.

use std::sync::LazyLock;

use html_escape::{encode_quoted_attribute, encode_text};
use regex::Regex;
use url::Url;

/// Marker substring on blocked/extract-failed responses. Used by the cache
/// writer to skip persisting these — we want a re-tap to retry, not return
/// the stale failure forever. The marker is an HTML comment so the rendered
/// page still looks normal to the user.
pub const BLOCKED_MARKER: &str = "<!--reader:blocked-->";

pub fn blocked_message(url: &str, reason: &str) -> String {
    format!(
        "{marker}<p><strong>{reason}</strong></p>\
         <p>The site refused to serve the page to the reader \
         (commonly: bot protection, paywall, or login required). \
         You can open the original on the Kindle browser:</p>\
         <p><a href='{url}'>{url}</a></p>",
        marker = BLOCKED_MARKER,
        reason = encode_text(reason),
        url = encode_text(url),
    )
}

/// Page body returned for URLs whose Content-Type is a non-HTML
/// resource (PDF, image, archive, etc.). Readability would otherwise
/// see opaque bytes, fail mysteriously, and the cache would fill with
/// "Could not extract readable content." entries. Linking to the
/// original lets the device's native viewer handle it.
pub fn binary_resource_message(url: &str, kind: &str) -> String {
    format!(
        "{marker}<p>This article is a <strong>{kind}</strong>, not a web page. \
         Open it directly in the device's viewer:</p>\
         <p><a href='{url}'>{url}</a></p>",
        marker = BLOCKED_MARKER,
        kind = encode_text(kind),
        url = encode_text(url),
    )
}

/// Map a `Content-Type` header value to a friendly resource label, or
/// `None` when the response looks like normal HTML/XHTML and should
/// flow through readability as usual. Anything else returns
/// `Some(label)` and bypasses extraction.
fn non_html_resource_kind(content_type: Option<&str>, url: &str) -> Option<String> {
    let ct = content_type.map(|c| c.split(';').next().unwrap_or(c).trim().to_ascii_lowercase());
    let by_header = ct.as_deref().and_then(|c| match c {
        "text/html" | "application/xhtml+xml" => None,
        "application/pdf" => Some("PDF document".to_string()),
        c if c.starts_with("image/") => Some(format!("{} image", c.trim_start_matches("image/"))),
        c if c.starts_with("video/") => Some("video".to_string()),
        c if c.starts_with("audio/") => Some("audio file".to_string()),
        "application/zip" | "application/x-zip-compressed" => Some("zip archive".to_string()),
        "application/octet-stream" => Some("binary file".to_string()),
        c if c.starts_with("text/") => None,
        c if c.starts_with("application/") && (c.contains("xml") || c.contains("json")) => None,
        _ => Some("non-HTML file".to_string()),
    });
    if by_header.is_some() {
        return by_header;
    }
    // Some servers omit Content-Type or serve PDFs with a generic
    // `application/octet-stream`; fall back to the URL path so a `.pdf`
    // tail still lands in the binary path.
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".pdf") {
        Some("PDF document".to_string())
    } else {
        None
    }
}

/// Fetch `url`, run readability, return clean HTML — or a blocked-message
/// HTML body if anything between the user's tap and a readable article
/// goes wrong. Never panics, never errors. The caller distinguishes blocked
/// from real content via `BLOCKED_MARKER`.
pub async fn extract_url(http: &reqwest::Client, url: &str) -> String {
    let resp = match http.get(url).send().await {
        Ok(r) => r,
        Err(e) => return blocked_message(url, &format!("Could not reach the site: {}", e)),
    };
    let status = resp.status();
    if status == 401 || status == 403 || status == 451 {
        return blocked_message(
            url,
            &format!("Site refused the request (HTTP {}).", status.as_u16()),
        );
    }
    if status.is_client_error() || status.is_server_error() {
        return blocked_message(url, &format!("Site returned HTTP {}.", status.as_u16()));
    }
    // Branch on Content-Type before consuming the body: a PDF (or any
    // non-HTML resource) goes straight to a link card, sparing
    // readability the opaque bytes and the user a wall of mojibake.
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(kind) = non_html_resource_kind(content_type.as_deref(), url) {
        return binary_resource_message(url, &kind);
    }
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => return blocked_message(url, &format!("Could not read body: {}", e)),
    };
    let parsed_url = match Url::parse(url) {
        Ok(u) => u,
        Err(_) => return blocked_message(url, "Could not parse URL."),
    };
    let mut cursor = std::io::Cursor::new(body.as_bytes());
    match readability::extractor::extract(&mut cursor, &parsed_url) {
        Ok(prod) if !prod.content.is_empty() => sanitize_images(&prod.content),
        _ => blocked_message(url, "Could not extract readable content."),
    }
}

// ---------------------------------------------------------------------------
// Image post-processing
//
// Article HTML often points at images the Kindle browser can't render:
// WebP, AVIF, oversized JPEGs that the device shrinks ungracefully,
// transparent PNGs that go black on a monochrome screen. To paper over
// all of that without making the browser do the work, every `http(s)`
// `<img>` is rewritten to point at the inkwell `/img` proxy. The proxy
// (see `crate::img`) fetches the source, downscales to a Kindle-sized
// box, re-encodes as JPEG under a tight size budget, and caches the
// result in SQLite.
//
// Strategy:
//   * `<img src="http(s)://…">`  →  `<img src="/img?u=…" style="…">`
//     with a `max-width:100%` style so the Kindle scales the result to
//     the column width regardless of the source dimensions.
//   * `<img>` with a non-http(s) src (data:, file:, mailto:, etc.) is
//     replaced with the alt-text fallback so the article still reads
//     top-to-bottom even though the image can't load.
//   * `<img>` with no src at all is dropped.

static IMG_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `<img ...>` or self-closing `<img ... />`. Attribute body captured.
    Regex::new(r#"(?is)<img\b([^>]*?)/?>"#).unwrap()
});

static SRC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());

static ALT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)\balt\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());

fn extract_attr(attrs: &str, re: &Regex) -> Option<String> {
    let c = re.captures(attrs)?;
    c.get(1)
        .or_else(|| c.get(2))
        .map(|m| m.as_str().to_string())
}

fn is_proxyable_src(src: &str) -> bool {
    src.starts_with("http://") || src.starts_with("https://")
}

pub fn sanitize_images(html: &str) -> String {
    IMG_RE
        .replace_all(html, |caps: &regex::Captures| {
            let attrs = &caps[1];
            let src = extract_attr(attrs, &SRC_RE).unwrap_or_default();
            if src.is_empty() {
                return String::new();
            }
            let alt = extract_attr(attrs, &ALT_RE).unwrap_or_default();
            if is_proxyable_src(&src) {
                // Route every http(s) image through the /img proxy. The
                // proxy URL-encodes the original URL, applies the SSRF
                // host guard, and serves a Kindle-sized JPEG.
                format!(
                    r#"<img src="/img?u={}" alt="{}" style="max-width:100%; height:auto">"#,
                    crate::view::url_encode(&src),
                    encode_quoted_attribute(&alt),
                )
            } else {
                let label = if alt.is_empty() {
                    "image".to_string()
                } else {
                    alt
                };
                format!(
                    r#"<div class="img-fallback">[{}]</div>"#,
                    encode_text(&label)
                )
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_message_contains_marker_and_url() {
        let msg = blocked_message("https://example.com/x", "HTTP 403");
        assert!(msg.contains(BLOCKED_MARKER));
        assert!(msg.contains("https://example.com/x"));
        assert!(msg.contains("HTTP 403"));
    }

    #[test]
    fn blocked_message_escapes_html() {
        let msg = blocked_message("https://example.com/?a=<x>", "got <b>403</b>");
        assert!(!msg.contains("<b>403"));
        assert!(msg.contains("&lt;b&gt;403&lt;/b&gt;"));
        // The URL is encoded as text inside the link body too.
        assert!(msg.contains("&lt;x&gt;"));
    }

    // ----- sanitize_images: http(s) → /img proxy rewrite ------------------

    #[test]
    fn sanitize_rewrites_http_jpg_through_img_proxy() {
        let out = sanitize_images(r#"<img src="https://x.com/p.jpg" alt="A photo">"#);
        assert!(
            out.contains(r#"src="/img?u=https%3A%2F%2Fx.com%2Fp.jpg""#),
            "expected proxy rewrite, got: {}",
            out
        );
        assert!(out.contains(r#"alt="A photo""#));
        assert!(out.contains(r#"style="max-width:100%; height:auto""#));
    }

    #[test]
    fn sanitize_rewrites_webp_and_avif_the_same_as_jpg() {
        // Format of the source URL no longer matters at sanitize time —
        // the /img proxy decodes anything and emits Kindle-friendly
        // JPEG. The old "webp → alt fallback" path is gone.
        for ext in ["webp", "avif", "svg", "gif", "png"] {
            let html = format!(r#"<img src="https://x.com/q.{}" alt="alt">"#, ext);
            let out = sanitize_images(&html);
            assert!(out.contains("/img?u="), "{}: {}", ext, out);
            assert!(!out.contains("img-fallback"), "{}: {}", ext, out);
        }
    }

    #[test]
    fn sanitize_preserves_alt_through_proxy_rewrite() {
        // Network failures on the proxy fetch are common over slow
        // mobile — the rewritten `<img>` must still carry alt so the
        // Kindle has something to show in the broken-image slot.
        let out = sanitize_images(r#"<img src="https://x.com/photo.jpg" alt="The cat">"#);
        assert!(out.contains(r#"alt="The cat""#));
    }

    #[test]
    fn sanitize_emits_empty_alt_when_source_has_none() {
        let out = sanitize_images(r#"<img src="https://x.com/icon.png">"#);
        assert!(out.contains(r#"alt="""#));
    }

    #[test]
    fn sanitize_url_encodes_query_string_in_proxy_target() {
        // `&` in the original src must survive into the `?u=` value as
        // `%26`, or the Kindle browser would parse it as a second
        // query parameter and the proxy would receive a truncated URL.
        let out = sanitize_images(
            r#"<img src="https://cdn.example.com/photo.jpg?w=600&fit=crop" alt="x">"#,
        );
        assert!(
            out.contains("%26"),
            "expected & to be percent-encoded: {}",
            out
        );
        assert!(
            !out.contains("?w=600&fit"),
            "raw & leaked into proxy URL: {}",
            out
        );
    }

    #[test]
    fn sanitize_keeps_data_uri_in_alt_fallback() {
        // The proxy only accepts http(s) sources; everything else
        // (data:, file:, etc.) falls back to the styled alt-text block
        // so the article still reads even though the image won't load.
        let out = sanitize_images(r#"<img src="data:image/webp;base64,xxx" alt="diagram">"#);
        assert!(out.contains("img-fallback"));
        assert!(out.contains("[diagram]"));
        assert!(!out.contains("/img?u="));
    }

    #[test]
    fn sanitize_uses_image_placeholder_when_non_http_src_has_no_alt() {
        let out = sanitize_images(r#"<img src="data:image/png;base64,xxx">"#);
        assert!(out.contains("[image]"));
    }

    #[test]
    fn sanitize_drops_imgs_with_no_src() {
        let out = sanitize_images(r#"before<img alt="x">after"#);
        assert_eq!(out, "beforeafter");
    }

    #[test]
    fn sanitize_handles_single_quoted_attributes() {
        let out = sanitize_images(r#"<img src='https://x.com/p.png' alt='hi'>"#);
        assert!(out.contains("/img?u=https%3A%2F%2Fx.com%2Fp.png"));
        assert!(out.contains(r#"alt="hi""#));
    }

    #[test]
    fn sanitize_escapes_alt_text_in_fallback() {
        let out = sanitize_images(r#"<img src="data:image/webp;base64,xx" alt="A & B">"#);
        assert!(out.contains("[A &amp; B]"));
    }

    #[test]
    fn sanitize_escapes_alt_text_in_proxy_rewrite() {
        // The proxy rewrite uses an attribute encoder for alt; a "
        // in the source must be encoded to `&quot;` or the attribute
        // would close early and inject markup.
        let out = sanitize_images(r#"<img src="https://x.com/p.jpg" alt='quote " here'>"#);
        assert!(!out.contains(r#"alt="quote " here""#));
        assert!(out.contains("&quot;"));
    }

    #[test]
    fn sanitize_handles_self_closing_tag() {
        let out = sanitize_images(r#"<img src="https://x.com/p.jpg" alt="x" />"#);
        assert!(out.contains("/img?u="));
    }

    // ----- non-HTML resource detection (issue #20) ------------------------

    #[test]
    fn non_html_kind_passes_through_html_and_xhtml() {
        assert!(non_html_resource_kind(Some("text/html"), "https://x/a").is_none());
        assert!(non_html_resource_kind(Some("text/html; charset=utf-8"), "https://x/a").is_none());
        assert!(non_html_resource_kind(Some("application/xhtml+xml"), "https://x/a").is_none());
    }

    #[test]
    fn non_html_kind_flags_pdf_by_content_type() {
        let kind = non_html_resource_kind(Some("application/pdf"), "https://x/paper").unwrap();
        assert_eq!(kind, "PDF document");
    }

    #[test]
    fn non_html_kind_flags_pdf_by_path_when_content_type_missing() {
        // The actual issue report: cl.cam.ac.uk serves a PDF where the
        // path ends with .pdf but the server's Content-Type may be
        // missing or generic.
        let kind = non_html_resource_kind(None, "https://www.cl.cam.ac.uk/~nk480/parsing.pdf");
        assert_eq!(kind.as_deref(), Some("PDF document"));
        // And the URL-tail rule still fires when the header is the
        // generic octet-stream that some servers emit.
        let kind = non_html_resource_kind(
            Some("application/octet-stream"),
            "https://x/paper.pdf?download=1",
        );
        assert!(
            kind.is_some(),
            "octet-stream PDF must not slip into readability"
        );
    }

    #[test]
    fn non_html_kind_flags_images_videos_audio_archives() {
        assert!(non_html_resource_kind(Some("image/jpeg"), "https://x/photo").is_some());
        assert!(non_html_resource_kind(Some("video/mp4"), "https://x/clip").is_some());
        assert!(non_html_resource_kind(Some("audio/mpeg"), "https://x/song").is_some());
        assert!(non_html_resource_kind(Some("application/zip"), "https://x/bundle").is_some());
    }

    #[test]
    fn non_html_kind_lets_xml_and_json_through_to_readability() {
        // Atom feeds, JSON-feed bodies, etc. — readability will fail
        // on them, but the blocked-message path is the right response,
        // not the "this is a binary file" card.
        assert!(non_html_resource_kind(Some("application/atom+xml"), "https://x/feed").is_none());
        assert!(non_html_resource_kind(Some("application/json"), "https://x/feed").is_none());
    }

    #[test]
    fn binary_resource_message_includes_marker_kind_and_link() {
        let out = binary_resource_message("https://x/paper.pdf", "PDF document");
        assert!(out.contains(BLOCKED_MARKER));
        assert!(out.contains("PDF document"));
        assert!(out.contains("href='https://x/paper.pdf'"));
    }

    #[test]
    fn binary_resource_message_escapes_kind_and_url() {
        // Same XSS-via-attribute regression class as blocked_message.
        let out = binary_resource_message("https://x/?a=<b>", "PDF <evil>");
        assert!(!out.contains("<evil>"));
        assert!(out.contains("&lt;evil&gt;"));
        assert!(out.contains("&lt;b&gt;"));
    }
}
