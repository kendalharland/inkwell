//! Live article fetch + readability extraction with a graceful blocked-site
//! fallback.

use html_escape::encode_text;
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
        Ok(prod) if !prod.content.is_empty() => prod.content,
        _ => blocked_message(url, "Could not extract readable content."),
    }
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
}
