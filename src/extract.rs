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
// The old Kindle browser only reliably renders JPEG, PNG, and GIF. WebP,
// AVIF, and SVG are increasingly common on modern sites — leaving them
// in-place produces a broken-image icon and the reader loses the
// information the picture was carrying.
//
// Strategy:
//   * Supported formats (jpg/jpeg/png/gif) are kept; we ensure an `alt`
//     attribute exists so the Kindle has fallback text if the network
//     fetch fails (slow 3G, captive portal, etc.).
//   * Unsupported formats are replaced with a styled `[alt text]` block
//     so the article still reads top-to-bottom. If the source had no
//     alt, we substitute "image" — better a sign-post than nothing.
//   * Images with no `src` at all are dropped.

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

fn is_simple_format(src: &str) -> bool {
    // Pull just the path component — strip query and fragment so
    // `photo.jpg?w=600` still classifies as a jpg.
    let lower = src.to_lowercase();
    let path = lower.split(['?', '#']).next().unwrap_or(&lower);
    path.ends_with(".jpg")
        || path.ends_with(".jpeg")
        || path.ends_with(".png")
        || path.ends_with(".gif")
}

pub fn sanitize_images(html: &str) -> String {
    IMG_RE
        .replace_all(html, |caps: &regex::Captures| {
            let attrs = &caps[1];
            let src = extract_attr(attrs, &SRC_RE).unwrap_or_default();
            if src.is_empty() {
                return String::new();
            }
            let alt = extract_attr(attrs, &ALT_RE);
            if is_simple_format(&src) {
                let alt_str = alt.unwrap_or_default();
                format!(
                    r#"<img src="{}" alt="{}">"#,
                    encode_quoted_attribute(&src),
                    encode_quoted_attribute(&alt_str)
                )
            } else {
                let label = alt
                    .filter(|a| !a.is_empty())
                    .unwrap_or_else(|| "image".into());
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

    #[test]
    fn sanitize_keeps_jpg_with_existing_alt() {
        let out = sanitize_images(r#"<img src="https://x.com/p.jpg" alt="A photo">"#);
        assert_eq!(out, r#"<img src="https://x.com/p.jpg" alt="A photo">"#);
    }

    #[test]
    fn sanitize_adds_empty_alt_when_missing() {
        let out = sanitize_images(r#"<img src="https://x.com/icon.png">"#);
        assert_eq!(out, r#"<img src="https://x.com/icon.png" alt="">"#);
    }

    #[test]
    fn sanitize_treats_jpeg_extension_the_same_as_jpg() {
        let out = sanitize_images(r#"<img src="https://x.com/a.jpeg" alt="x">"#);
        assert!(out.starts_with("<img"));
    }

    #[test]
    fn sanitize_classifies_query_string_url_by_path_extension() {
        let out = sanitize_images(
            r#"<img src="https://cdn.example.com/photo.jpg?w=600&fit=crop" alt="x">"#,
        );
        assert!(
            out.starts_with("<img"),
            "expected <img> for jpg with query string, got: {}",
            out
        );
    }

    #[test]
    fn sanitize_replaces_webp_with_alt_fallback() {
        let out = sanitize_images(r#"<img src="https://x.com/q.webp" alt="diagram of A">"#);
        assert!(out.contains("<div class=\"img-fallback\">"));
        assert!(out.contains("[diagram of A]"));
        assert!(!out.contains("<img"));
    }

    #[test]
    fn sanitize_replaces_avif_with_image_placeholder_when_no_alt() {
        let out = sanitize_images(r#"<img src="https://x.com/q.avif">"#);
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
        assert!(out.contains(r#"src="https://x.com/p.png""#));
        assert!(out.contains(r#"alt="hi""#));
    }

    #[test]
    fn sanitize_escapes_alt_text_in_fallback() {
        // Raw `&` in attributes is technically tolerated by HTML5.
        let out = sanitize_images(r#"<img src="https://x.com/q.webp" alt="A & B">"#);
        assert!(out.contains("[A &amp; B]"));
    }

    #[test]
    fn sanitize_handles_self_closing_tag() {
        let out = sanitize_images(r#"<img src="https://x.com/p.jpg" alt="x" />"#);
        assert!(out.starts_with("<img"));
    }

    #[test]
    fn sanitize_treats_svg_as_unsupported() {
        let out = sanitize_images(r#"<img src="https://x.com/logo.svg" alt="logo">"#);
        assert!(out.contains("img-fallback"));
        assert!(out.contains("[logo]"));
    }

    #[test]
    fn sanitize_treats_data_uri_as_unsupported() {
        // Data URIs vary; treat as unsupported by default since they're
        // rarely simple-format raster images.
        let out = sanitize_images(r#"<img src="data:image/webp;base64,xxx" alt="x">"#);
        assert!(out.contains("img-fallback"));
    }
}
