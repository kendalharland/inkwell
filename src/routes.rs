//! axum handlers. Each one is thin: collect → render. Heavy lifting lives
//! in [`crate::feeds`], [`crate::extract`], and [`crate::view`].

use std::{fmt::Write as _, sync::Arc};

use axum::{
    extract::{Form, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Router,
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use html_escape::encode_text;
use serde::Deserialize;
use url::Url;

use crate::{
    admin,
    extract::{extract_url, sanitize_images, BLOCKED_MARKER},
    feeds::{collect_entries, ensure_feeds, entry_full_html, item_id},
    state::AppState,
    view::{now_secs, page, render_entries, url_encode},
};

#[derive(Deserialize)]
pub struct PageQ {
    #[serde(default = "default_page")]
    pub page: usize,
    /// `?compact=1` forces compact density on this request without
    /// touching the cookie. Useful for one-shot testing and sharing.
    #[serde(default)]
    pub compact: Option<u8>,
    /// `?theme=dark` / `?theme=light` overrides without touching the cookie.
    #[serde(default)]
    pub theme: Option<String>,
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
    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Deserialize)]
struct CompactSettingQ {
    /// `1` = enable, `0` = disable, absent = toggle.
    #[serde(default)]
    to: Option<u8>,
    /// Path to redirect back to. Must start with `/`.
    #[serde(default)]
    from: Option<String>,
}

#[derive(Deserialize)]
struct ThemeSettingQ {
    /// `dark` / `light`, absent = toggle.
    #[serde(default)]
    to: Option<String>,
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
        .route("/settings/theme", get(set_theme))
        .route("/admin", get(admin_index))
        .route("/admin/feed/add", post(admin_add_feed))
        .route("/admin/feed/remove", post(admin_remove_feed))
        .route("/admin/group/add", post(admin_add_group))
        .route("/admin/group/remove", post(admin_remove_group))
        .with_state(state)
}

/// Precedence for both compact and theme: explicit query param wins
/// over the sticky cookie, which wins over the config default.
fn effective_compact(jar: &CookieJar, q: Option<u8>, default: bool) -> bool {
    let from_query = q.map(|n| n != 0);
    let from_cookie = jar.get("compact").and_then(|c| match c.value() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    });
    from_query.or(from_cookie).unwrap_or(default)
}

fn effective_dark(jar: &CookieJar, q: Option<&str>, default: bool) -> bool {
    let from_query = q.and_then(|v| match v.to_ascii_lowercase().as_str() {
        "dark" | "1" => Some(true),
        "light" | "0" => Some(false),
        _ => None,
    });
    let from_cookie = jar.get("theme").and_then(|c| match c.value() {
        "dark" => Some(true),
        "light" => Some(false),
        _ => None,
    });
    from_query.or(from_cookie).unwrap_or(default)
}

/// Composes the `<body class="...">` attribute value from the active
/// density and theme preferences. Empty string when neither is on.
fn body_classes(
    jar: &CookieJar,
    q_compact: Option<u8>,
    q_theme: Option<&str>,
    state: &AppState,
) -> String {
    let mut classes: Vec<&str> = Vec::new();
    if effective_compact(jar, q_compact, state.compact_default) {
        classes.push("compact");
    }
    if effective_dark(jar, q_theme, state.dark_default) {
        classes.push("dark");
    }
    classes.join(" ")
}

/// Prefer an explicit `from` (must start with `/`); fall back to the
/// Referer header so a nav-link click lands back on the same page;
/// finally root.
fn back_url(headers: &HeaderMap, from: Option<&str>) -> String {
    from.filter(|s| s.starts_with('/'))
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("referer")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| Url::parse(s).ok())
                .map(|u| {
                    let p = u.path().to_string();
                    let q = u.query().map(|q| format!("?{}", q)).unwrap_or_default();
                    format!("{}{}", p, q)
                })
        })
        .unwrap_or_else(|| "/".into())
}

fn sticky_cookie(name: &str, value: &str) -> Cookie<'static> {
    Cookie::build((name.to_string(), value.to_string()))
        .path("/")
        .max_age(time::Duration::days(365))
        .http_only(true)
        .build()
}

async fn set_compact(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    jar: CookieJar,
    Query(q): Query<CompactSettingQ>,
) -> impl IntoResponse {
    let current = effective_compact(&jar, None, state.compact_default);
    let next = match q.to {
        Some(1) => true,
        Some(0) => false,
        _ => !current,
    };
    let jar = jar.add(sticky_cookie("compact", if next { "1" } else { "0" }));
    (jar, Redirect::to(&back_url(&headers, q.from.as_deref())))
}

async fn set_theme(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    jar: CookieJar,
    Query(q): Query<ThemeSettingQ>,
) -> impl IntoResponse {
    let current = effective_dark(&jar, None, state.dark_default);
    let next = match q.to.as_deref().map(|s| s.to_ascii_lowercase()) {
        Some(s) if s == "dark" || s == "1" => true,
        Some(s) if s == "light" || s == "0" => false,
        _ => !current,
    };
    let jar = jar.add(sticky_cookie("theme", if next { "dark" } else { "light" }));
    (jar, Redirect::to(&back_url(&headers, q.from.as_deref())))
}

async fn all_stories(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Query(q): Query<PageQ>,
) -> Html<String> {
    let bc = body_classes(&jar, q.compact, q.theme.as_deref(), &state);
    let feeds = state.feeds.read().await.clone();
    let all_idxs: Vec<usize> = (0..feeds.len()).collect();
    ensure_feeds(state.clone(), &all_idxs, false).await;
    let cache = state.feed_cache.read().await;
    let entries = collect_entries(&feeds, &cache, &all_idxs);
    let body = render_entries("All stories", &entries, q.page, "/", true);
    Html(page("All stories", &body, &bc))
}

async fn feeds_list(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Query(q): Query<PageQ>,
) -> Html<String> {
    let bc = body_classes(&jar, q.compact, q.theme.as_deref(), &state);
    let feeds = state.feeds.read().await.clone();
    let all_idxs: Vec<usize> = (0..feeds.len()).collect();
    ensure_feeds(state.clone(), &all_idxs, false).await;
    let titles = state.feed_titles.read().await;
    let mut body = String::from("<h1>Feeds</h1><ul class='list'>");
    for (i, url) in feeds.iter().enumerate() {
        let title = titles.get(i).and_then(|t| t.as_deref()).unwrap_or(url);
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
    Html(page("Feeds", &body, &bc))
}

async fn one_feed(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    AxumPath(idx): AxumPath<usize>,
    Query(q): Query<PageQ>,
) -> Result<Html<String>, StatusCode> {
    let feeds = state.feeds.read().await.clone();
    if idx >= feeds.len() {
        return Err(StatusCode::NOT_FOUND);
    }
    let bc = body_classes(&jar, q.compact, q.theme.as_deref(), &state);
    ensure_feeds(state.clone(), &[idx], false).await;
    let cache = state.feed_cache.read().await;
    let title = cache
        .get(&idx)
        .and_then(|c| c.parsed.title.as_ref().map(|t| t.content.clone()))
        .unwrap_or_else(|| feeds[idx].clone());
    let entries = collect_entries(&feeds, &cache, &[idx]);
    let body = render_entries(&title, &entries, q.page, &format!("/feed/{}", idx), false);
    Ok(Html(page(&title, &body, &bc)))
}

async fn groups_list(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Query(q): Query<PageQ>,
) -> Html<String> {
    let bc = body_classes(&jar, q.compact, q.theme.as_deref(), &state);
    let groups = state.groups.read().await.clone();
    let mut body = String::from("<h1>Groups</h1><ul class='list'>");
    for (i, g) in groups.iter().enumerate() {
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
    Html(page("Groups", &body, &bc))
}

async fn one_group(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    AxumPath(idx): AxumPath<usize>,
    Query(q): Query<PageQ>,
) -> Result<Html<String>, StatusCode> {
    let (name, indices) = {
        let groups = state.groups.read().await;
        let Some(g) = groups.get(idx) else {
            return Err(StatusCode::NOT_FOUND);
        };
        (g.name.clone(), g.feed_indices.clone())
    };
    let bc = body_classes(&jar, q.compact, q.theme.as_deref(), &state);
    let feeds = state.feeds.read().await.clone();
    ensure_feeds(state.clone(), &indices, false).await;
    let cache = state.feed_cache.read().await;
    let entries = collect_entries(&feeds, &cache, &indices);
    let body = render_entries(&name, &entries, q.page, &format!("/group/{}", idx), true);
    Ok(Html(page(&name, &body, &bc)))
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
    let bc = body_classes(&jar, q.compact, q.theme.as_deref(), &state);
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
        let (link, title, full) = found.ok_or(StatusCode::NOT_FOUND)?;
        // Sanitize either path: the live extractor does its own pass, but
        // feed-provided full HTML can carry WebP/AVIF/SVG too. The
        // sanitizer is idempotent.
        let extracted = if let Some(h) = full {
            sanitize_images(&h)
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
    Ok(Html(page(&title, &body, &bc)))
}

// ---------------------------------------------------------------------------
// Admin

#[derive(Deserialize)]
struct AdminQ {
    /// Flash message shown above the form (success).
    #[serde(default)]
    ok: Option<String>,
    /// Flash message shown above the form (error).
    #[serde(default)]
    err: Option<String>,
}

#[derive(Deserialize)]
struct FeedForm {
    group: String,
    url: String,
}

#[derive(Deserialize)]
struct GroupForm {
    name: String,
}

#[derive(Deserialize)]
struct RemoveGroupForm {
    name: String,
}

async fn admin_index(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Query(q): Query<AdminQ>,
) -> Result<Html<String>, StatusCode> {
    let groups = admin::list_groups(&state).await.map_err(|e| {
        tracing::error!("admin: list_groups failed: {e:#}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let bc = body_classes(&jar, None, None, &state);

    let flash = match (&q.ok, &q.err) {
        (Some(m), _) => format!("<div class='flash flash-ok'>{}</div>", encode_text(m)),
        (_, Some(m)) => format!("<div class='flash flash-err'>{}</div>", encode_text(m)),
        _ => String::new(),
    };

    let mut groups_html = String::new();
    for g in &groups {
        write!(
            groups_html,
            "<section class='admin-group'><h2>{name}</h2>",
            name = encode_text(&g.name)
        )
        .unwrap();
        if g.feeds.is_empty() {
            groups_html.push_str("<p class='empty'>(no feeds)</p>");
        } else {
            groups_html.push_str("<ul class='list'>");
            for url in &g.feeds {
                write!(
                    groups_html,
                    "<li><span class='meta'>{url}</span>\
                     <form method='post' action='/admin/feed/remove' class='inline'>\
                     <input type='hidden' name='group' value='{group}'>\
                     <input type='hidden' name='url' value='{url}'>\
                     <button type='submit'>Remove</button>\
                     </form></li>",
                    url = encode_text(url),
                    group = encode_text(&g.name),
                )
                .unwrap();
            }
            groups_html.push_str("</ul>");
        }
        write!(
            groups_html,
            "<form method='post' action='/admin/feed/add' class='inline'>\
             <input type='hidden' name='group' value='{group}'>\
             <input type='url' name='url' placeholder='https://example.com/feed.xml' required size='40'>\
             <button type='submit'>Add feed</button>\
             </form>\
             <form method='post' action='/admin/group/remove' class='inline' onsubmit=\"return confirm('Remove group {group_js} and its feeds?');\">\
             <input type='hidden' name='name' value='{group}'>\
             <button type='submit'>Remove group</button>\
             </form></section>",
            group = encode_text(&g.name),
            group_js = encode_text(&g.name).replace('\'', ""),
        )
        .unwrap();
    }

    let body = crate::template::render(
        include_str!("templates/admin.html"),
        &[("flash", &flash), ("groups", &groups_html)],
    );
    Ok(Html(page("Admin", &body, &bc)))
}

fn redirect_with_flash(ok: Option<&str>, err: Option<&str>) -> Redirect {
    let mut url = String::from("/admin");
    if let Some(m) = ok {
        url.push_str("?ok=");
        url.push_str(&url_encode(m));
    } else if let Some(m) = err {
        url.push_str("?err=");
        url.push_str(&url_encode(m));
    }
    Redirect::to(&url)
}

async fn admin_add_feed(
    State(state): State<Arc<AppState>>,
    Form(f): Form<FeedForm>,
) -> Redirect {
    let group = f.group.clone();
    let url = f.url.clone();
    match admin::add_feed_to_group(&state, &group, &url).await {
        Ok(()) => redirect_with_flash(Some(&format!("Added {} to {}.", url, group)), None),
        Err(e) => redirect_with_flash(None, Some(&format!("Could not add feed: {}", e))),
    }
}

async fn admin_remove_feed(
    State(state): State<Arc<AppState>>,
    Form(f): Form<FeedForm>,
) -> Redirect {
    let group = f.group.clone();
    let url = f.url.clone();
    match admin::remove_feed_from_group(&state, &group, &url).await {
        Ok(()) => redirect_with_flash(Some(&format!("Removed {} from {}.", url, group)), None),
        Err(e) => redirect_with_flash(None, Some(&format!("Could not remove feed: {}", e))),
    }
}

async fn admin_add_group(
    State(state): State<Arc<AppState>>,
    Form(f): Form<GroupForm>,
) -> Redirect {
    let name = f.name.clone();
    match admin::add_group(&state, &name).await {
        Ok(()) => redirect_with_flash(Some(&format!("Created group {}.", name)), None),
        Err(e) => redirect_with_flash(None, Some(&format!("Could not create group: {}", e))),
    }
}

async fn admin_remove_group(
    State(state): State<Arc<AppState>>,
    Form(f): Form<RemoveGroupForm>,
) -> Redirect {
    let name = f.name.clone();
    match admin::remove_group(&state, &name).await {
        Ok(()) => redirect_with_flash(Some(&format!("Removed group {}.", name)), None),
        Err(e) => redirect_with_flash(None, Some(&format!("Could not remove group: {}", e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jar() -> CookieJar {
        CookieJar::new()
    }

    fn jar_with(name: &str, value: &str) -> CookieJar {
        CookieJar::new().add(Cookie::new(name.to_string(), value.to_string()))
    }

    #[test]
    fn effective_compact_default_when_unset() {
        assert!(!effective_compact(&make_jar(), None, false));
        assert!(effective_compact(&make_jar(), None, true));
    }

    #[test]
    fn effective_compact_cookie_overrides_default() {
        assert!(effective_compact(&jar_with("compact", "1"), None, false));
        assert!(!effective_compact(&jar_with("compact", "0"), None, true));
    }

    #[test]
    fn effective_compact_query_overrides_cookie() {
        assert!(!effective_compact(&jar_with("compact", "1"), Some(0), false));
        assert!(effective_compact(&jar_with("compact", "0"), Some(1), false));
    }

    #[test]
    fn effective_compact_ignores_unknown_cookie_value() {
        let jar = jar_with("compact", "yes");
        assert!(effective_compact(&jar, None, true));
        assert!(!effective_compact(&jar, None, false));
    }

    #[test]
    fn effective_dark_default_when_unset() {
        assert!(!effective_dark(&make_jar(), None, false));
        assert!(effective_dark(&make_jar(), None, true));
    }

    #[test]
    fn effective_dark_cookie_overrides_default() {
        assert!(effective_dark(&jar_with("theme", "dark"), None, false));
        assert!(!effective_dark(&jar_with("theme", "light"), None, true));
    }

    #[test]
    fn effective_dark_query_overrides_cookie() {
        // The cookie says dark but a `?theme=light` query forces light here.
        assert!(!effective_dark(&jar_with("theme", "dark"), Some("light"), false));
        assert!(effective_dark(&jar_with("theme", "light"), Some("dark"), false));
    }

    #[test]
    fn effective_dark_accepts_1_and_0_aliases() {
        assert!(effective_dark(&make_jar(), Some("1"), false));
        assert!(!effective_dark(&make_jar(), Some("0"), true));
    }
}
