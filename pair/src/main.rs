//! # inkwell-pair
//!
//! Tiny sidecar that mints one-time short codes for "log this device in
//! the next time it visits". Designed to run alongside an auth gateway
//! like authelia: the operator visits `/generate-token` from an
//! already-authenticated browser, reads the 6-digit code off the
//! screen, then types `/token/<code>` on the new device — the sidecar
//! sets a cookie that the auth gateway honors and redirects to the
//! configured app URL.
//!
//! Storage is in-memory by default (a `HashMap<code, issued_at>`);
//! losing it on restart is harmless because tokens are short-lived.
//! Redis support could be a feature-gated addition later.
//!
//! Every behavior is env-var driven so the same image can serve
//! authelia, custom forward-auth shims, or any other cookie-shaped
//! session model. See the README under `pair/` for the full env-var
//! surface.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use rand::Rng;
use tokio::sync::Mutex;

/// Runtime configuration, frozen at startup. Every field maps 1:1 to
/// an env var documented in `pair/README.md`; no YAML, no config file
/// — the sidecar is meant to ship as a single Docker image whose
/// behavior is fully described by its environment.
#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub bind: String,

    /// Where /token/<code> redirects to on success.
    pub redirect_url: String,

    /// How long a freshly minted code is valid.
    pub token_ttl: Duration,

    pub cookie_name: String,
    pub cookie_value: String,
    pub cookie_domain: Option<String>,
    pub cookie_path: String,
    pub cookie_max_age: Duration,
    pub cookie_secure: bool,
    pub cookie_http_only: bool,
    /// `Strict` / `Lax` / `None`. Case is preserved as-is into the header.
    pub cookie_same_site: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Config {
            port: parse_env("PAIR_PORT", "3000")?,
            bind: env_or("PAIR_BIND", "0.0.0.0"),
            redirect_url: std::env::var("PAIR_REDIRECT_URL")
                .context("PAIR_REDIRECT_URL is required (where /token/<code> sends users on success)")?,
            token_ttl: Duration::from_secs(parse_env("PAIR_TOKEN_TTL_SECS", "300")?),
            cookie_name: env_or("PAIR_COOKIE_NAME", "authelia_session"),
            cookie_value: env_or("PAIR_COOKIE_VALUE", "valid"),
            cookie_domain: std::env::var("PAIR_COOKIE_DOMAIN").ok().filter(|s| !s.is_empty()),
            cookie_path: env_or("PAIR_COOKIE_PATH", "/"),
            cookie_max_age: Duration::from_secs(parse_env("PAIR_COOKIE_MAX_AGE_SECS", "2592000")?),
            cookie_secure: parse_bool_env("PAIR_COOKIE_SECURE", true)?,
            cookie_http_only: parse_bool_env("PAIR_COOKIE_HTTP_ONLY", true)?,
            cookie_same_site: env_or("PAIR_COOKIE_SAME_SITE", "Lax"),
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn parse_env<T: std::str::FromStr>(key: &str, default: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    let raw = std::env::var(key).unwrap_or_else(|_| default.to_string());
    raw.parse::<T>()
        .map_err(|e| anyhow::anyhow!("{} = {:?}: {}", key, raw, e))
}

fn parse_bool_env(key: &str, default: bool) -> Result<bool> {
    let raw = match std::env::var(key) {
        Ok(v) => v,
        Err(_) => return Ok(default),
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => anyhow::bail!("{} = {:?}: expected boolean", key, raw),
    }
}

/// In-memory store of `code -> when_issued`. Expiry is checked at use
/// time and also opportunistically swept on insertion so a brute-force
/// attacker can't grow the map.
#[derive(Default)]
pub struct TokenStore {
    inner: Mutex<HashMap<String, Instant>>,
}

impl TokenStore {
    pub async fn mint(&self, code: String, now: Instant, ttl: Duration) {
        let mut g = self.inner.lock().await;
        g.retain(|_, t| now.duration_since(*t) < ttl);
        g.insert(code, now);
    }

    /// Atomically remove the code iff it exists and is still fresh.
    /// Returns true if a valid code was consumed.
    pub async fn consume(&self, code: &str, now: Instant, ttl: Duration) -> bool {
        let mut g = self.inner.lock().await;
        match g.remove(code) {
            Some(issued) if now.duration_since(issued) <= ttl => true,
            // Expired: drop it (we already removed) and report invalid.
            _ => false,
        }
    }

    #[cfg(test)]
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<TokenStore>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/generate-token", get(generate_token))
        .route("/token/{code}", get(use_token))
        .with_state(state)
}

/// Format the 6-digit code. Random in `[0, 999_999]` zero-padded.
pub fn fresh_code() -> String {
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{:06}", n)
}

/// Strict shape check matching what `fresh_code` emits.
pub fn is_valid_code(s: &str) -> bool {
    s.len() == 6 && s.chars().all(|c| c.is_ascii_digit())
}

async fn generate_token(State(s): State<AppState>) -> Html<String> {
    let code = fresh_code();
    s.store
        .mint(code.clone(), Instant::now(), s.config.token_ttl)
        .await;
    let ttl_min = s.config.token_ttl.as_secs() / 60;
    // The code itself is rendered inside <code id="pair-code"> so tests
    // (and future scrapers) can target it unambiguously — the prose
    // also uses <code> for the path example, which would otherwise
    // collide.
    Html(format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Pair code</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:32em;margin:3em auto;padding:0 1em;}}\
         #pair-code{{display:block;font-size:3em;letter-spacing:.15em;text-align:center;padding:.4em;background:#f4f4f4;border-radius:.2em;}}\
         </style></head><body>\
         <h1>Pair this device</h1>\
         <p>On your new device, visit <code>/token/&lt;code&gt;</code> within {ttl} minutes:</p>\
         <code id=\"pair-code\">{code}</code></body></html>",
        code = code,
        ttl = ttl_min,
    ))
}

async fn use_token(
    State(s): State<AppState>,
    Path(code): Path<String>,
) -> Result<Response, StatusCode> {
    if !is_valid_code(&code) {
        return Err(StatusCode::NOT_FOUND);
    }
    let ok = s
        .store
        .consume(&code, Instant::now(), s.config.token_ttl)
        .await;
    if !ok {
        return Err(StatusCode::NOT_FOUND);
    }
    let header_value = build_set_cookie(&s.config)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut resp = Redirect::to(&s.config.redirect_url).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, header_value);
    Ok(resp)
}

fn build_set_cookie(cfg: &Config) -> Result<HeaderValue, ()> {
    let mut s = format!(
        "{name}={value}; Path={path}; Max-Age={max_age}",
        name = cfg.cookie_name,
        value = cfg.cookie_value,
        path = cfg.cookie_path,
        max_age = cfg.cookie_max_age.as_secs(),
    );
    if let Some(domain) = &cfg.cookie_domain {
        s.push_str(&format!("; Domain={}", domain));
    }
    if cfg.cookie_secure {
        s.push_str("; Secure");
    }
    if cfg.cookie_http_only {
        s.push_str("; HttpOnly");
    }
    s.push_str(&format!("; SameSite={}", cfg.cookie_same_site));
    HeaderValue::from_str(&s).map_err(|_| ())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Arc::new(Config::from_env().context("loading config from env")?);
    tracing::info!(
        "inkwell-pair starting on {}:{} — redirect target: {}, cookie: {}",
        config.bind,
        config.port,
        config.redirect_url,
        config.cookie_name,
    );
    let state = AppState {
        config: config.clone(),
        store: Arc::new(TokenStore::default()),
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind((config.bind.as_str(), config.port)).await?;
    let serve = axum::serve(listener, app);
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("ctrl-c — shutting down");
    };
    tokio::select! {
        r = serve => r?,
        _ = shutdown => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_config() -> Config {
        Config {
            port: 3000,
            bind: "127.0.0.1".into(),
            redirect_url: "https://app.example.com/".into(),
            token_ttl: Duration::from_secs(60),
            cookie_name: "authelia_session".into(),
            cookie_value: "valid".into(),
            cookie_domain: Some(".example.com".into()),
            cookie_path: "/".into(),
            cookie_max_age: Duration::from_secs(86400),
            cookie_secure: true,
            cookie_http_only: true,
            cookie_same_site: "Lax".into(),
        }
    }

    fn test_state() -> AppState {
        AppState {
            config: Arc::new(test_config()),
            store: Arc::new(TokenStore::default()),
        }
    }

    async fn body_to_string(resp: Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn extract_code_from_html(html: &str) -> String {
        // Target the labeled element — the page also uses <code> in
        // the prose explaining how to redeem.
        let needle = "<code id=\"pair-code\">";
        let start = html.find(needle).unwrap() + needle.len();
        let end = html[start..].find("</code>").unwrap();
        html[start..start + end].to_string()
    }

    #[tokio::test]
    async fn fresh_code_is_six_digits() {
        for _ in 0..100 {
            let c = fresh_code();
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[tokio::test]
    async fn is_valid_code_accepts_six_digits_and_rejects_everything_else() {
        assert!(is_valid_code("000000"));
        assert!(is_valid_code("123456"));
        assert!(!is_valid_code(""));
        assert!(!is_valid_code("12345"));
        assert!(!is_valid_code("1234567"));
        assert!(!is_valid_code("12345a"));
        assert!(!is_valid_code("../etc/passwd"));
    }

    #[tokio::test]
    async fn generate_then_use_flow_sets_cookie_and_redirects() {
        let state = test_state();

        // GET /generate-token → 200 HTML, store has 1 entry.
        let resp = router(state.clone())
            .oneshot(Request::get("/generate-token").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_to_string(resp).await;
        let code = extract_code_from_html(&html);
        assert!(is_valid_code(&code), "extracted bad code: {:?}", code);
        assert_eq!(state.store.len().await, 1);

        // GET /token/<code> → 303, Set-Cookie present, store now empty.
        let resp = router(state.clone())
            .oneshot(
                Request::get(format!("/token/{}", code))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = resp.headers().get(header::LOCATION).unwrap();
        assert_eq!(loc, "https://app.example.com/");
        let cookie = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(cookie.starts_with("authelia_session=valid"), "cookie was {}", cookie);
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Domain=.example.com"));
        assert!(cookie.contains("Max-Age=86400"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));

        // Token is single-use — second attempt 404s.
        let resp = router(state.clone())
            .oneshot(
                Request::get(format!("/token/{}", code))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(state.store.len().await, 0);
    }

    #[tokio::test]
    async fn use_token_rejects_unknown_code() {
        let app = router(test_state());
        let resp = app
            .oneshot(Request::get("/token/999999").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn use_token_rejects_malformed_codes_without_db_lookup() {
        // Inputs cover: too-short, alphabetic, percent-encoded HTML
        // injection, and percent-encoded path traversal. We URL-encode
        // anything not in the unreserved set so `Request::get` doesn't
        // refuse them at build time — what we're testing is the handler,
        // not the URI parser.
        let app = router(test_state());
        for path in [
            "/token/abc",
            "/token/12345",
            "/token/1234567", // too long
            "/token/%3Cscript%3E",
            "/token/..%2F..%2Fetc%2Fpasswd",
        ] {
            let resp = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{} should 404", path);
        }
    }

    #[tokio::test]
    async fn expired_tokens_cannot_be_consumed() {
        let store = Arc::new(TokenStore::default());
        let ttl = Duration::from_millis(1);
        store
            .mint("123456".into(), Instant::now(), ttl)
            .await;
        // Wait past TTL.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!store.consume("123456", Instant::now(), ttl).await);
    }

    #[tokio::test]
    async fn mint_evicts_expired_entries() {
        let store = TokenStore::default();
        let ttl = Duration::from_millis(1);
        // Drop in a stale entry, then mint a fresh one — the fresh
        // mint should sweep the stale.
        store.mint("aaa".into(), Instant::now() - Duration::from_secs(60), ttl).await;
        store.mint("bbb".into(), Instant::now(), ttl).await;
        // Only the fresh entry survives.
        assert_eq!(store.len().await, 1);
    }

    #[tokio::test]
    async fn build_set_cookie_omits_optional_attributes_when_disabled() {
        let mut cfg = test_config();
        cfg.cookie_domain = None;
        cfg.cookie_secure = false;
        cfg.cookie_http_only = false;
        let v = build_set_cookie(&cfg).unwrap();
        let s = v.to_str().unwrap();
        assert!(!s.contains("Domain="));
        assert!(!s.contains("Secure"));
        assert!(!s.contains("HttpOnly"));
        assert!(s.contains("SameSite=Lax"));
    }

    #[tokio::test]
    async fn parse_bool_env_handles_common_shapes() {
        for v in ["1", "true", "TRUE", "yes", "on"] {
            std::env::set_var("INKWELL_PAIR_TEST_BOOL", v);
            assert!(parse_bool_env("INKWELL_PAIR_TEST_BOOL", false).unwrap());
        }
        for v in ["0", "false", "FALSE", "no", "off"] {
            std::env::set_var("INKWELL_PAIR_TEST_BOOL", v);
            assert!(!parse_bool_env("INKWELL_PAIR_TEST_BOOL", true).unwrap());
        }
        std::env::set_var("INKWELL_PAIR_TEST_BOOL", "maybe");
        assert!(parse_bool_env("INKWELL_PAIR_TEST_BOOL", true).is_err());
        std::env::remove_var("INKWELL_PAIR_TEST_BOOL");
        assert!(parse_bool_env("INKWELL_PAIR_TEST_BOOL", true).unwrap());
    }
}
