//! axum handlers. Each one is thin: collect → render. Heavy lifting lives
//! in [`crate::feeds`], [`crate::extract`], and [`crate::view`].

use std::{fmt::Write as _, sync::Arc};

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::Html,
    routing::get,
    Router,
};
use html_escape::encode_text;
use serde::Deserialize;
use url::Url;

use crate::{
    extract::{extract_url, BLOCKED_MARKER},
    feeds::{collect_entries, ensure_feeds, entry_full_html, item_id},
    state::AppState,
    view::{now_secs, page, render_entries},
};

#[derive(Deserialize)]
pub struct PageQ {
    #[serde(default = "default_page")]
    pub page: usize,
}
fn default_page() -> usize {
    1
}

#[derive(Deserialize)]
pub struct ItemQ {
    #[serde(default)]
    pub from: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(all_stories))
        .route("/feeds", get(feeds_list))
        .route("/feed/{idx}", get(one_feed))
        .route("/groups", get(groups_list))
        .route("/group/{idx}", get(one_group))
        .route("/item/{iid}", get(one_item))
        .with_state(state)
}

async fn all_stories(State(state): State<Arc<AppState>>, Query(q): Query<PageQ>) -> Html<String> {
    let all_idxs: Vec<usize> = (0..state.feeds.len()).collect();
    ensure_feeds(state.clone(), &all_idxs, false).await;
    let cache = state.feed_cache.read().await;
    let entries = collect_entries(&state, &cache, &all_idxs);
    let body = render_entries("All stories", &entries, q.page, "/", true);
    Html(page("All stories", &body, ""))
}

async fn feeds_list(State(state): State<Arc<AppState>>) -> Html<String> {
    let all_idxs: Vec<usize> = (0..state.feeds.len()).collect();
    ensure_feeds(state.clone(), &all_idxs, false).await;
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
    Html(page("Feeds", &body, ""))
}

async fn one_feed(
    State(state): State<Arc<AppState>>,
    AxumPath(idx): AxumPath<usize>,
    Query(q): Query<PageQ>,
) -> Result<Html<String>, StatusCode> {
    if idx >= state.feeds.len() {
        return Err(StatusCode::NOT_FOUND);
    }
    ensure_feeds(state.clone(), &[idx], false).await;
    let cache = state.feed_cache.read().await;
    let title = cache
        .get(&idx)
        .and_then(|c| c.parsed.title.as_ref().map(|t| t.content.clone()))
        .unwrap_or_else(|| state.feeds[idx].clone());
    let entries = collect_entries(&state, &cache, &[idx]);
    let body = render_entries(&title, &entries, q.page, &format!("/feed/{}", idx), false);
    Ok(Html(page(&title, &body, "")))
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
    Html(page("Groups", &body, ""))
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
    ensure_feeds(state.clone(), &indices, false).await;
    let cache = state.feed_cache.read().await;
    let entries = collect_entries(&state, &cache, &indices);
    let body = render_entries(&name, &entries, q.page, &format!("/group/{}", idx), true);
    Ok(Html(page(&name, &body, "")))
}

/// Article render. Tries the persistent SQLite cache first (so a feed that
/// has rolled off still serves), falls back to looking up the entry in the
/// live feed cache and either using the feed's full-text body or doing a
/// live extract. Blocked responses are surfaced to the user but not cached.
///
/// `?from=` carries the page the user came from so the Back button works
/// across All-stories / feed / group views. Validated to start with `/`
/// before being interpolated into an `href` to prevent `javascript:` XSS.
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
    let body = crate::template::render(
        include_str!("templates/item.html"),
        &[
            ("title", &encode_text(&title)),
            ("link", &encode_text(&link)),
            ("host", &encode_text(&host)),
            ("body", &body_html),
            ("back", &encode_text(&back)),
        ],
    );
    Ok(Html(page(&title, &body, "")))
}
