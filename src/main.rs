use std::{
    collections::HashMap,
    fmt::Write as _,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::Html,
    routing::get,
    Router,
};
use feed_rs::{model::Entry, model::Feed as ParsedFeed, parser};
use futures::future::join_all;
use html_escape::encode_text;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use tokio::sync::{Mutex, RwLock};
use url::Url;

const PAGE_SIZE: usize = 25;
const BLOCKED_MARKER: &str = "<!--reader:blocked-->";

const STYLE: &str = r#"
body{font-family:Georgia,serif;font-size:20px;line-height:1.5;max-width:40em;
margin:1em auto;padding:0 1em;color:#000;background:#fff}
a{color:#000}
nav{border-bottom:2px solid #000;padding-bottom:.5em;margin-bottom:1em;font-size:17px}
nav a{margin-right:1.2em;text-decoration:none}
h1{font-size:26px;margin:.5em 0}
h2{font-size:22px}
ul.list{list-style:none;padding:0;margin:0}
ul.list li{border-bottom:1px solid #888;padding:.9em 0}
ul.list a{display:block;font-size:22px;text-decoration:none}
.meta{color:#444;font-size:15px;margin-top:.2em}
.empty{padding:2em 0;color:#555}
.actions{margin:2em 0 4em;padding-top:1em;border-top:2px solid #000}
.actions a.btn,.pager a{display:inline-block;padding:.7em 1.2em;border:2px solid #000;
background:#fff;font-size:18px;margin:.3em .3em 0 0;text-decoration:none;color:#000}
.pager{margin:1em 0 2em}
.pager span{margin:0 .5em;font-size:16px}
img{max-width:100%;height:auto}
pre{white-space:pre-wrap;word-wrap:break-word}
blockquote{border-left:3px solid #888;margin:.8em 0;padding-left:.8em;color:#222}
.err{border:2px solid #000;padding:1em;background:#eee}
"#;

#[derive(Debug, Deserialize)]
struct ConfigFile {
    rss: RssConfig,
}

#[derive(Debug, Deserialize)]
struct RssConfig {
    groups: Vec<GroupConfig>,
}

#[derive(Debug, Deserialize)]
struct GroupConfig {
    name: String,
    feeds: Vec<String>,
}

struct GroupInfo {
    name: String,
    feed_indices: Vec<usize>,
}

struct CachedFeed {
    parsed: ParsedFeed,
    fetched_at: SystemTime,
}

struct AppState {
    feeds: Vec<String>,
    feed_titles: RwLock<Vec<Option<String>>>,
    groups: Vec<GroupInfo>,
    http: reqwest::Client,
    feed_cache: RwLock<HashMap<usize, CachedFeed>>,
    db: Mutex<rusqlite::Connection>,
    feed_ttl: Duration,
}

struct EntryView {
    iid: String,
    title: String,
    host: String,
    published_ts: i64,
    feed_title: String,
}

fn item_id(link: &str) -> String {
    let mut h = Sha1::new();
    h.update(link.as_bytes());
    let bytes = h.finalize();
    let hex = format!("{:x}", bytes);
    hex.chars().take(16).collect()
}

fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn fetch_feed_once(http: &reqwest::Client, url: &str) -> Result<ParsedFeed> {
    let resp = http.get(url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    let parsed = parser::parse(bytes.as_ref())?;
    Ok(parsed)
}

async fn ensure_feeds(state: Arc<AppState>, indices: &[usize]) {
    let now = SystemTime::now();
    let to_fetch: Vec<usize> = {
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

    let jobs = to_fetch.into_iter().map(|i| {
        let state = state.clone();
        async move {
            let url = state.feeds[i].clone();
            match fetch_feed_once(&state.http, &url).await {
                Ok(parsed) => Some((i, parsed)),
                Err(e) => {
                    tracing::warn!("feed {} fetch failed: {}", url, e);
                    None
                }
            }
        }
    });
    let results = join_all(jobs).await;

    let mut cache = state.feed_cache.write().await;
    let mut titles = state.feed_titles.write().await;
    let fetched_at = SystemTime::now();
    for opt in results {
        if let Some((i, parsed)) = opt {
            if let Some(t) = parsed.title.as_ref().map(|t| t.content.clone()) {
                titles[i] = Some(t);
            }
            cache.insert(i, CachedFeed { parsed, fetched_at });
        }
    }
}

fn collect_entries(
    state: &AppState,
    cache: &HashMap<usize, CachedFeed>,
    indices: &[usize],
) -> Vec<EntryView> {
    let mut out = Vec::new();
    for &i in indices {
        let Some(cf) = cache.get(&i) else { continue };
        let feed_title = cf
            .parsed
            .title
            .as_ref()
            .map(|t| t.content.clone())
            .unwrap_or_else(|| state.feeds[i].clone());
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
                published_ts,
                feed_title: feed_title.clone(),
            });
        }
    }
    out.sort_by(|a, b| b.published_ts.cmp(&a.published_ts));
    out
}

fn entry_full_html(e: &Entry) -> Option<String> {
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

fn blocked_message(url: &str, reason: &str) -> String {
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

async fn extract_url(http: &reqwest::Client, url: &str) -> String {
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

fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head>\
         <meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>{style}</style></head>\
         <body><nav>\
         <a href='/'>All stories</a> \
         <a href='/feeds'>Feeds</a> \
         <a href='/groups'>Groups</a>\
         </nav>{body}</body></html>",
        title = encode_text(title),
        style = STYLE,
        body = body,
    )
}

fn render_entries(
    title: &str,
    entries: &[EntryView],
    page_num: usize,
    base_path: &str,
    show_source: bool,
) -> String {
    let page_num = page_num.max(1);
    let total = entries.len();
    let total_pages = total.div_ceil(PAGE_SIZE).max(1);
    let page_num = page_num.min(total_pages);
    let start = (page_num - 1) * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(total);

    let mut body = format!("<h1>{}</h1>", encode_text(title));
    if entries.is_empty() {
        body.push_str("<div class='empty'>No items.</div>");
        return body;
    }
    body.push_str("<ul class='list'>");
    let from = format!("{}?page={}", base_path, page_num);
    let from_enc = url_encode(&from);
    for e in &entries[start..end] {
        let source = if show_source {
            format!(" — {}", encode_text(&e.feed_title))
        } else {
            String::new()
        };
        write!(
            body,
            "<li><a href='/item/{iid}?from={from}'>{title}</a>\
             <div class='meta'>{host}{source}</div></li>",
            iid = e.iid,
            from = from_enc,
            title = encode_text(&e.title),
            host = encode_text(&e.host),
            source = source,
        )
        .unwrap();
    }
    body.push_str("</ul>");
    if total_pages > 1 {
        body.push_str("<div class='pager'>");
        if page_num > 1 {
            write!(
                body,
                "<a href='{base}?page={p}'>Previous</a>",
                base = base_path,
                p = page_num - 1
            )
            .unwrap();
        }
        write!(
            body,
            "<span>Page {} of {}</span>",
            page_num, total_pages
        )
        .unwrap();
        if page_num < total_pages {
            write!(
                body,
                "<a href='{base}?page={p}'>Next</a>",
                base = base_path,
                p = page_num + 1
            )
            .unwrap();
        }
        body.push_str("</div>");
    }
    body
}

#[derive(Deserialize)]
struct PageQ {
    #[serde(default = "default_page")]
    page: usize,
}
fn default_page() -> usize {
    1
}

#[derive(Deserialize)]
struct ItemQ {
    #[serde(default)]
    from: Option<String>,
}

async fn all_stories(State(state): State<Arc<AppState>>, Query(q): Query<PageQ>) -> Html<String> {
    let all_idxs: Vec<usize> = (0..state.feeds.len()).collect();
    ensure_feeds(state.clone(), &all_idxs).await;
    let cache = state.feed_cache.read().await;
    let entries = collect_entries(&state, &cache, &all_idxs);
    let body = render_entries("All stories", &entries, q.page, "/", true);
    Html(page("All stories", &body))
}

async fn feeds_list(State(state): State<Arc<AppState>>) -> Html<String> {
    let all_idxs: Vec<usize> = (0..state.feeds.len()).collect();
    ensure_feeds(state.clone(), &all_idxs).await;
    let titles = state.feed_titles.read().await;
    let mut body = String::from("<h1>Feeds</h1><ul class='list'>");
    for (i, url) in state.feeds.iter().enumerate() {
        let title = titles[i].as_deref().unwrap_or(url);
        write!(
            body,
            "<li><a href='/feed/{i}'>{title}</a><div class='meta'>{url}</div></li>",
            i = i,
            title = encode_text(title),
            url = encode_text(url),
        )
        .unwrap();
    }
    body.push_str("</ul>");
    Html(page("Feeds", &body))
}

async fn one_feed(
    State(state): State<Arc<AppState>>,
    AxumPath(idx): AxumPath<usize>,
    Query(q): Query<PageQ>,
) -> Result<Html<String>, StatusCode> {
    if idx >= state.feeds.len() {
        return Err(StatusCode::NOT_FOUND);
    }
    ensure_feeds(state.clone(), &[idx]).await;
    let cache = state.feed_cache.read().await;
    let title = cache
        .get(&idx)
        .and_then(|c| c.parsed.title.as_ref().map(|t| t.content.clone()))
        .unwrap_or_else(|| state.feeds[idx].clone());
    let entries = collect_entries(&state, &cache, &[idx]);
    let body = render_entries(&title, &entries, q.page, &format!("/feed/{}", idx), false);
    Ok(Html(page(&title, &body)))
}

async fn groups_list(State(state): State<Arc<AppState>>) -> Html<String> {
    let mut body = String::from("<h1>Groups</h1><ul class='list'>");
    for (i, g) in state.groups.iter().enumerate() {
        let count = g.feed_indices.len();
        write!(
            body,
            "<li><a href='/group/{i}'>{name}</a><div class='meta'>{count} feed{s}</div></li>",
            i = i,
            name = encode_text(&g.name),
            count = count,
            s = if count == 1 { "" } else { "s" },
        )
        .unwrap();
    }
    body.push_str("</ul>");
    Html(page("Groups", &body))
}

async fn one_group(
    State(state): State<Arc<AppState>>,
    AxumPath(idx): AxumPath<usize>,
    Query(q): Query<PageQ>,
) -> Result<Html<String>, StatusCode> {
    let Some(group) = state.groups.get(idx) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let indices = group.feed_indices.clone();
    let name = group.name.clone();
    ensure_feeds(state.clone(), &indices).await;
    let cache = state.feed_cache.read().await;
    let entries = collect_entries(&state, &cache, &indices);
    let body = render_entries(&name, &entries, q.page, &format!("/group/{}", idx), true);
    Ok(Html(page(&name, &body)))
}

async fn one_item(
    State(state): State<Arc<AppState>>,
    AxumPath(iid): AxumPath<String>,
    Query(q): Query<ItemQ>,
) -> Result<Html<String>, StatusCode> {
    let back = q
        .from
        .as_deref()
        .filter(|s| s.starts_with('/'))
        .unwrap_or("/")
        .to_string();

    let cached = {
        let conn = state.db.lock().await;
        conn.query_row(
            "SELECT url, title, html FROM article WHERE id=?1",
            [&iid],
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
        let all_idxs: Vec<usize> = (0..state.feeds.len()).collect();
        ensure_feeds(state.clone(), &all_idxs).await;
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
        let (link, title, full) = found.ok_or(StatusCode::NOT_FOUND)?;
        let extracted = if let Some(h) = full {
            h
        } else {
            extract_url(&state.http, &link).await
        };
        if !extracted.contains(BLOCKED_MARKER) {
            let conn = state.db.lock().await;
            let _ = conn.execute(
                "INSERT OR REPLACE INTO article (id,url,title,html,fetched_at) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![&iid, &link, &title, &extracted, now_secs()],
            );
        }
        (link, title, extracted)
    };

    let host = Url::parse(&link)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_default();
    let body = format!(
        "<h1>{title}</h1><div class='meta'><a href='{link}'>{host}</a></div>{body}\
         <div class='actions'><a class='btn' href='{back}'>Back</a></div>",
        title = encode_text(&title),
        link = encode_text(&link),
        host = encode_text(&host),
        body = body_html,
        back = encode_text(&back),
    );
    Ok(Html(page(&title, &body)))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path: PathBuf = std::env::args()
        .nth(1)
        .context("usage: inkwell <config.yaml>")?
        .into();
    let config_str = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config: ConfigFile = serde_yml::from_str(&config_str)
        .with_context(|| format!("parsing {}", config_path.display()))?;

    let mut feeds: Vec<String> = Vec::new();
    let mut url_to_idx: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<GroupInfo> = Vec::new();
    for g in config.rss.groups {
        let mut idxs = Vec::new();
        for url in g.feeds {
            let idx = *url_to_idx.entry(url.clone()).or_insert_with(|| {
                let i = feeds.len();
                feeds.push(url);
                i
            });
            idxs.push(idx);
        }
        groups.push(GroupInfo {
            name: g.name,
            feed_indices: idxs,
        });
    }
    if feeds.is_empty() {
        anyhow::bail!("no feeds configured");
    }

    let feed_titles = RwLock::new(vec![None; feeds.len()]);

    let timeout_secs: u64 = std::env::var("HTTP_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; inkwell-rss-reader/0.1)")
        .timeout(Duration::from_secs(timeout_secs))
        .build()?;

    let db_path = std::env::var("CACHE_DB").unwrap_or_else(|_| "reader_cache.sqlite".into());
    let conn = rusqlite::Connection::open(&db_path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS article (
            id TEXT PRIMARY KEY,
            url TEXT,
            title TEXT,
            html TEXT,
            fetched_at INTEGER
        )",
    )?;

    let feed_ttl = Duration::from_secs(
        std::env::var("FEED_TTL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600),
    );

    let state = Arc::new(AppState {
        feeds,
        feed_titles,
        groups,
        http,
        feed_cache: RwLock::new(HashMap::new()),
        db: Mutex::new(conn),
        feed_ttl,
    });

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5050);
    let app = Router::new()
        .route("/", get(all_stories))
        .route("/feeds", get(feeds_list))
        .route("/feed/{idx}", get(one_feed))
        .route("/groups", get(groups_list))
        .route("/group/{idx}", get(one_group))
        .route("/item/{iid}", get(one_item))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("listening on http://0.0.0.0:{}", port);
    axum::serve(listener, app).await?;
    Ok(())
}
