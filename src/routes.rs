//! axum handlers. Each one is thin: collect → render. Heavy lifting lives
//! in [`crate::feeds`], [`crate::extract`], and [`crate::view`].

use std::{fmt::Write as _, sync::Arc};

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect},
    routing::get,
    Router,
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
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
    /// `?compact=1` forces compact density on this request without
    /// touching the cookie. Useful for one-shot testing.
    #[serde(default)]
    pub compact: Option<u8>,
}
fn default_page() -> usize {
    1
}

#[derive(Deserialize)]
pub struct ItemQ {
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub compact: Option<u8>,
}

#[derive(Deserialize)]
struct CompactSettingQ {
    /// `1` = enable, `0` = disable, absent = toggle.
    #[serde(default)]
    to: Option<u8>,
    /// Path to redirect back to after toggling. Must start with `/`.
    #[serde(default)]
    from: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(all_stories))
        .route("/feeds", get(feeds_list))
        .route("/feed/{idx}", get(one_feed))
        .route("/groups", get(groups_list))
        .route("/group/{idx}", get(one_group))
        .route("/item/{iid}", get(one_item))
        .route("/settings/compact", get(set_compact))
        .with_state(state)
}

/// Precedence: explicit `?compact=` on this request wins; otherwise the
/// sticky cookie; otherwise the config default. Returning a `&'static str`
/// avoids an allocation in the common path.
fn body_class(jar: &CookieJar, q: Option<u8>, default: bool) -> &'static str {
    let from_query = q.map(|n| n != 0);
    let from_cookie = jar.get("compact").and_then(|c| match c.value() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    });
    if from_query.or(from_cookie).unwrap_or(default) {
        "compact"
    } else {
        ""
    }
}

/// Toggle (or set) the sticky compact-density cookie and redirect the user
/// back to where they came from. `?to=1` / `?to=0` set explicitly; absent
/// `to` flips whatever the cookie currently holds (config default is
/// treated as "off" for the toggle purpose so the user always reaches a
/// definitive state after one click).
async fn set_compact(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    jar: CookieJar,
    Query(q): Query<CompactSettingQ>,
) -> impl IntoResponse {
    let current = jar
        .get("compact")
        .and_then(|c| match c.value() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        })
        .unwrap_or(state.compact_default);
    let next = match q.to {
        Some(1) => true,
        Some(0) => false,
        _ => !current,
    };
    let cookie = Cookie::build((
        "compact",
        if next { "1" } else { "0" },
    ))
    .path("/")
    .max_age(time::Duration::days(365))
    .http_only(true)
    .build();
    let jar = jar.add(cookie);

    // Prefer an explicit `from` so the user can deep-link; fall back to
    // the Referer header (present from the nav-link click on most
    // browsers); finally fall back to the home page.
    let back = q
        .from
        .as_deref()
        .filter(|s| s.starts_with('/'))
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("referer")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| Url::parse(s).ok())
                .and_then(|u| {
                    let p = u.path().to_string();
                    let q = u.query().map(|q| format!("?{}", q)).unwrap_or_default();
                    Some(format!("{}{}", p, q))
                })
        })
        .unwrap_or_else(|| "/".into());
    (jar, Redirect::to(&back))
}

async fn all_stories(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Query(q): Query<PageQ>,
) -> Html<String> {
    let bc = body_class(&jar, q.compact, state.compact_default);
    let all_idxs: Vec<usize> = (0..state.feeds.len()).collect();
    ensure_feeds(state.clone(), &all_idxs, false).await;
    let cache = state.feed_cache.read().await;
    let entries = collect_entries(&state, &cache, &all_idxs);
    let body = render_entries("All stories", &entries, q.page, "/", true);
    Html(page("All stories", &body, bc))
}

async fn feeds_list(State(state): State<Arc<AppState>>, jar: CookieJar, Query(q): Query<PageQ>) -> Html<String> {
    let bc = body_class(&jar, q.compact, state.compact_default);
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
    Html(page("Feeds", &body, bc))
}

async fn one_feed(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    AxumPath(idx): AxumPath<usize>,
    Query(q): Query<PageQ>,
) -> Result<Html<String>, StatusCode> {
    if idx >= state.feeds.len() {
        return Err(StatusCode::NOT_FOUND);
    }
    let bc = body_class(&jar, q.compact, state.compact_default);
    ensure_feeds(state.clone(), &[idx], false).await;
    let cache = state.feed_cache.read().await;
    let title = cache
        .get(&idx)
        .and_then(|c| c.parsed.title.as_ref().map(|t| t.content.clone()))
        .unwrap_or_else(|| state.feeds[idx].clone());
    let entries = collect_entries(&state, &cache, &[idx]);
    let body = render_entries(&title, &entries, q.page, &format!("/feed/{}", idx), false);
    Ok(Html(page(&title, &body, bc)))
}

async fn groups_list(State(state): State<Arc<AppState>>, jar: CookieJar, Query(q): Query<PageQ>) -> Html<String> {
    let bc = body_class(&jar, q.compact, state.compact_default);
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
    Html(page("Groups", &body, bc))
}

async fn one_group(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    AxumPath(idx): AxumPath<usize>,
    Query(q): Query<PageQ>,
) -> Result<Html<String>, StatusCode> {
    let Some(group) = state.groups.get(idx) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let bc = body_class(&jar, q.compact, state.compact_default);
    let indices = group.feed_indices.clone();
    let name = group.name.clone();
    ensure_feeds(state.clone(), &indices, false).await;
    let cache = state.feed_cache.read().await;
    let entries = collect_entries(&state, &cache, &indices);
    let body = render_entries(&name, &entries, q.page, &format!("/group/{}", idx), true);
    Ok(Html(page(&name, &body, bc)))
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
    jar: CookieJar,
    AxumPath(iid): AxumPath<String>,
    Query(q): Query<ItemQ>,
) -> Result<Html<String>, StatusCode> {
    let bc = body_class(&jar, q.compact, state.compact_default);
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
    Ok(Html(page(&title, &body, bc)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jar() -> CookieJar {
        CookieJar::new()
    }

    fn jar_with_compact(value: &str) -> CookieJar {
        CookieJar::new().add(Cookie::new("compact", value.to_string()))
    }

    #[test]
    fn body_class_uses_default_when_unset() {
        assert_eq!(body_class(&make_jar(), None, false), "");
        assert_eq!(body_class(&make_jar(), None, true), "compact");
    }

    #[test]
    fn body_class_cookie_overrides_default() {
        assert_eq!(body_class(&jar_with_compact("1"), None, false), "compact");
        assert_eq!(body_class(&jar_with_compact("0"), None, true), "");
    }

    #[test]
    fn body_class_query_overrides_cookie() {
        assert_eq!(body_class(&jar_with_compact("1"), Some(0), false), "");
        assert_eq!(body_class(&jar_with_compact("0"), Some(1), false), "compact");
    }

    #[test]
    fn body_class_ignores_unknown_cookie_value() {
        let jar = CookieJar::new().add(Cookie::new("compact", "yes"));
        // Falls through to default since the cookie value isn't 1 or 0.
        assert_eq!(body_class(&jar, None, true), "compact");
        assert_eq!(body_class(&jar, None, false), "");
    }
}
