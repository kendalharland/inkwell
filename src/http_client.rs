//! Shared HTTP client defaults.
//!
//! A handful of large publishers (NYT, Psyche, others fronted by
//! aggressive bot-detection) refuse to serve any User-Agent that
//! looks like an aggregator — anything containing `bot`, `crawler`,
//! `rss`, or the `Mozilla/5.0 (compatible; <name>)` shape that older
//! RSS readers traditionally used. They want the request to look like
//! it's coming from a real browser before they'll return HTML.
//!
//! The fix is conservative: use a stable, recent Chrome-on-macOS UA
//! string and send the `Accept` / `Accept-Language` headers any real
//! browser includes by default. We don't try to mimic per-site
//! quirks; getting the basics right is enough to unblock the sites in
//! the bug reports.

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE};

/// Browser-shaped User-Agent. Sites that gate on UA pattern-matching
/// (vs. JavaScript challenges) accept this; sites that gate on JS
/// challenges block every UA equally and the article extractor's
/// blocked-message path catches that.
pub const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// `Accept` value a current Chrome sends for a top-level page load.
pub const ACCEPT_HEADER: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,\
     image/webp,image/apng,*/*;q=0.8";

/// `Accept-Language`: English-first, broadly readable elsewhere. Real
/// browsers send the user's locale here; a stable, plausible value is
/// good enough to clear most heuristics.
pub const ACCEPT_LANGUAGE_HEADER: &str = "en-US,en;q=0.9";

/// `default_headers` to attach to every request. Includes the
/// non-UA headers; UA is set separately via `ClientBuilder::user_agent`
/// so reqwest's own UA tracking applies.
pub fn default_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(ACCEPT, HeaderValue::from_static(ACCEPT_HEADER));
    h.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE_HEADER),
    );
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_ua_does_not_self_identify_as_a_bot() {
        // Regression guard for the original report — the previous UA
        // was `Mozilla/5.0 (compatible; inkwell-rss-reader/0.1)`, and
        // every substring below is a known pattern bot-walls match on
        // (case-insensitively). If any future edit re-introduces one,
        // this test fails loudly.
        let ua = BROWSER_UA.to_ascii_lowercase();
        for needle in ["bot", "crawler", "rss", "reader", "inkwell", "compatible;"] {
            assert!(
                !ua.contains(needle),
                "UA still contains {:?}: {}",
                needle,
                BROWSER_UA
            );
        }
    }

    #[test]
    fn browser_ua_has_a_recognizable_browser_signature() {
        // Real browsers' UAs include `Mozilla/5.0` and at least one of
        // the rendering-engine markers (AppleWebKit / Gecko / Trident).
        // Either alone is rare in the wild; pin both so a stripped-down
        // edit can't sneak through.
        assert!(BROWSER_UA.starts_with("Mozilla/5.0"));
        assert!(BROWSER_UA.contains("AppleWebKit"));
        assert!(BROWSER_UA.contains("Chrome") || BROWSER_UA.contains("Safari"));
    }

    #[test]
    fn default_headers_include_accept_and_accept_language() {
        let h = default_headers();
        assert!(h.contains_key(ACCEPT));
        assert!(h.contains_key(ACCEPT_LANGUAGE));
    }

    #[test]
    fn default_headers_accept_is_html_first() {
        // The article-extract path expects HTML back; if a site
        // content-negotiates on Accept it must return the HTML
        // variant, not JSON or any image fallback.
        let h = default_headers();
        let accept = h.get(ACCEPT).unwrap().to_str().unwrap();
        let html_pos = accept.find("text/html").expect("text/html missing");
        // text/html must appear before any */* fallback so the server
        // picks HTML when it has both available.
        let star_pos = accept.find("*/*");
        if let Some(star) = star_pos {
            assert!(html_pos < star, "Accept lists */* before text/html");
        }
    }

    // ----- live header round-trip -----------------------------------------

    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spin up a one-shot HTTP listener on 127.0.0.1, accept a single
    /// request, return the raw request bytes the client sent (so a
    /// test can assert on the actual wire headers reqwest emits).
    async fn capture_one_request() -> (TcpListener, std::net::SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    async fn read_request_send_204(listener: TcpListener) -> String {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let mut total = 0;
        // Read until we see the end-of-headers marker. A single read()
        // is sufficient for the small requests reqwest generates here.
        loop {
            let n = socket.read(&mut buf[total..]).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
            if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        socket
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        let _ = socket.shutdown().await;
        String::from_utf8_lossy(&buf[..total]).into_owned()
    }

    fn build_test_client() -> reqwest::Client {
        // Mirror the production client builder in main.rs.
        reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .default_headers(default_headers())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn client_sends_browser_user_agent_on_the_wire() {
        let (listener, addr) = capture_one_request().await;
        let server = tokio::spawn(read_request_send_204(listener));
        let client = build_test_client();
        let resp = client
            .get(format!("http://{}/page", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 204);
        let req = server.await.unwrap();
        assert!(
            req.contains("Mozilla/5.0"),
            "User-Agent missing browser prefix:\n{}",
            req
        );
        assert!(
            req.contains("AppleWebKit"),
            "User-Agent missing WebKit marker:\n{}",
            req
        );
        // Regression guard for the original bug — the previous UA
        // self-identified as an aggregator, which bot-walls block.
        assert!(
            !req.to_lowercase().contains("inkwell-rss-reader"),
            "old aggregator UA leaked into a real request:\n{}",
            req
        );
    }

    #[tokio::test]
    async fn client_sends_accept_and_accept_language_on_the_wire() {
        let (listener, addr) = capture_one_request().await;
        let server = tokio::spawn(read_request_send_204(listener));
        let client = build_test_client();
        client
            .get(format!("http://{}/page", addr))
            .send()
            .await
            .unwrap();
        let req = server.await.unwrap();
        assert!(
            req.to_lowercase().contains("accept: text/html"),
            "Accept header missing or wrong:\n{}",
            req
        );
        assert!(
            req.to_lowercase().contains("accept-language:"),
            "Accept-Language header missing:\n{}",
            req
        );
        assert!(
            req.contains("en-US"),
            "Accept-Language doesn't include en-US:\n{}",
            req
        );
    }
}
