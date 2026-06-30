//! axum handlers. Each one is thin: collect → render. Heavy lifting lives
//! in [`crate::feeds`], [`crate::extract`], and [`crate::view`].

use std::{fmt::Write as _, sync::Arc};

use axum::{
    extract::{Form, Multipart, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Json, Router,
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use html_escape::{encode_single_quoted_attribute, encode_text};
use serde::Deserialize;
use url::Url;

use crate::{
    admin, bookmarks,
    extract::{extract_url, sanitize_images, BLOCKED_MARKER},
    feed_search,
    feeds::{collect_entries, ensure_feeds, entry_full_html, is_valid_iid, item_id},
    img,
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
        .route("/read-later", get(read_later))
        .route("/bookmark/{iid}", post(add_bookmark))
        .route("/unbookmark/{iid}", post(remove_bookmark))
        .route("/settings/compact", get(set_compact))
        .route("/settings/theme", get(set_theme))
        .route("/admin", get(admin_index))
        .route("/admin/feed/add", post(admin_add_feed))
        .route("/admin/feed/remove", post(admin_remove_feed))
        .route("/admin/feed-search", get(admin_feed_search))
        .route("/admin/group/add", post(admin_add_group))
        .route("/admin/group/remove", post(admin_remove_group))
        .route("/admin/import-opml", post(admin_import_opml))
        .route("/img", get(image_proxy))
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

/// Same as [`back_url`] but with `#item-{iid}` appended so the browser
/// scrolls back to the row the user just toggled instead of the top of
/// the page. Drops any pre-existing fragment on the back URL — only
/// one anchor wins, and the row anchor is the one that matters here.
fn back_url_with_item_anchor(headers: &HeaderMap, from: Option<&str>, iid: &str) -> String {
    let mut url = back_url(headers, from);
    if let Some(hash) = url.find('#') {
        url.truncate(hash);
    }
    url.push_str("#item-");
    url.push_str(iid);
    url
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
    let bms = {
        let conn = state.db.lock().await;
        bookmarks::load_ids(&conn)
    };
    let body = render_entries("All stories", &entries, &bms, q.page, "/", true);
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
    let bms = {
        let conn = state.db.lock().await;
        bookmarks::load_ids(&conn)
    };
    let body = render_entries(
        &title,
        &entries,
        &bms,
        q.page,
        &format!("/feed/{}", idx),
        false,
    );
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
    let bms = {
        let conn = state.db.lock().await;
        bookmarks::load_ids(&conn)
    };
    let body = render_entries(
        &name,
        &entries,
        &bms,
        q.page,
        &format!("/group/{}", idx),
        true,
    );
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
    // Reject any iid that doesn't match the shape `feeds::item_id`
    // produces. Without this, an attacker-controlled path param can
    // reach the bookmark form's HTML interpolation (see #16).
    if !is_valid_iid(&iid) {
        return Err(StatusCode::NOT_FOUND);
    }
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
                    let l = e.links.first().map(|l| l.href.clone()).unwrap_or_default();
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
    let bookmarked = {
        let conn = state.db.lock().await;
        bookmarks::is_bookmarked(&conn, &iid)
    };
    let bookmark_btn = render_item_bookmark_button(&iid, &link, &title, &back, bookmarked);
    let body = crate::template::render(
        include_str!("templates/item.html"),
        &[
            ("title", &encode_text(&title)),
            ("link", &encode_text(&link)),
            ("host", &encode_text(&host)),
            ("body", &body_html),
            ("back", &encode_text(&back)),
            ("bookmark", &bookmark_btn),
        ],
    );
    Ok(Html(page(&title, &body, &bc)))
}

fn render_item_bookmark_button(
    iid: &str,
    url: &str,
    title: &str,
    from: &str,
    bookmarked: bool,
) -> String {
    // Attribute values are single-quoted; use the attribute-specific
    // encoder so an apostrophe in the title/url/from can't close the
    // attribute (#17).
    let (action, label) = if bookmarked {
        ("/unbookmark", "Remove bookmark")
    } else {
        ("/bookmark", "Save for later")
    };
    format!(
        "<form method='post' action='{action}/{iid}' class='bookmark-form bookmark-form-lg'>\
         <input type='hidden' name='url' value='{url}'>\
         <input type='hidden' name='title' value='{title}'>\
         <input type='hidden' name='from' value='{from}'>\
         <button type='submit' class='bookmark-btn bookmark-btn-lg' aria-label='{label}'>{icon}</button>\
         </form>",
        action = action,
        iid = iid,
        url = encode_single_quoted_attribute(url),
        title = encode_single_quoted_attribute(title),
        from = encode_single_quoted_attribute(from),
        label = label,
        icon = crate::view::bookmark_icon(bookmarked),
    )
}

#[derive(Deserialize)]
struct BookmarkForm {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    from: Option<String>,
}

async fn add_bookmark(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(iid): AxumPath<String>,
    Form(f): Form<BookmarkForm>,
) -> Result<Redirect, StatusCode> {
    if !is_valid_iid(&iid) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let title = f.title.trim();
    // Reject anything that isn't an http(s) URL. Without this,
    // `javascript:`/`data:` slip in and `/item/<iid>` later renders
    // them in `<a href="…">`; encode_text doesn't strip the scheme,
    // so clicking the host link runs the attacker's script (#18).
    let url = match admin::validate_feed_url(&f.url) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("bookmark add rejected for {}: {e}", iid);
            return Ok(Redirect::to(&back_url_with_item_anchor(
                &headers,
                f.from.as_deref(),
                &iid,
            )));
        }
    };
    if !title.is_empty() {
        let conn = state.db.lock().await;
        if let Err(e) = bookmarks::add(&conn, &iid, url, title, now_secs()) {
            tracing::error!("bookmark add failed for {}: {e:#}", iid);
        }
    }
    Ok(Redirect::to(&back_url_with_item_anchor(
        &headers,
        f.from.as_deref(),
        &iid,
    )))
}

async fn remove_bookmark(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(iid): AxumPath<String>,
    Form(f): Form<BookmarkForm>,
) -> Result<Redirect, StatusCode> {
    if !is_valid_iid(&iid) {
        return Err(StatusCode::BAD_REQUEST);
    }
    {
        let conn = state.db.lock().await;
        if let Err(e) = bookmarks::remove(&conn, &iid) {
            tracing::error!("bookmark remove failed for {}: {e:#}", iid);
        }
    }
    Ok(Redirect::to(&back_url_with_item_anchor(
        &headers,
        f.from.as_deref(),
        &iid,
    )))
}

async fn read_later(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Query(q): Query<PageQ>,
) -> Html<String> {
    let bc = body_classes(&jar, q.compact, q.theme.as_deref(), &state);
    let items_data = {
        let conn = state.db.lock().await;
        bookmarks::list(&conn).unwrap_or_default()
    };
    if items_data.is_empty() {
        let body = "<h1>Read later</h1><div class='empty'>No bookmarks yet. \
                    Tap the bookmark icon next to a story to save it for later.</div>";
        return Html(page("Read later", body, &bc));
    }
    let mut items = String::from("<h1>Read later</h1><ul class='list'>");
    let from_enc = url_encode("/read-later");
    let icon = crate::view::bookmark_icon(true);
    for b in &items_data {
        let host = Url::parse(&b.url)
            .ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_default();
        write!(
            items,
            "<li class='entry' id='item-{iid}'>\
             <form method='post' action='/unbookmark/{iid}' class='bookmark-form'>\
             <input type='hidden' name='from' value='{from_path}'>\
             <button type='submit' class='bookmark-btn' aria-label='Remove bookmark'>{icon}</button>\
             </form>\
             <div class='entry-body'>\
             <a href='/item/{iid}?from={from}'>{title}</a>\
             <div class='meta'>{host}</div>\
             </div></li>",
            iid = b.article_id,
            from = from_enc,
            from_path = encode_text("/read-later"),
            title = encode_text(&b.title),
            host = encode_text(&host),
            icon = icon,
        )
        .unwrap();
    }
    items.push_str("</ul>");
    Html(page("Read later", &items, &bc))
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
        let name_attr = encode_single_quoted_attribute(&g.name);
        let name_text = encode_text(&g.name);
        // Group heading: name + small × delete icon. The confirmation
        // prompt scrubs the single-quote out of the name so it can't
        // close the JS string passed to confirm().
        let group_js = name_text.replace('\'', "");
        write!(
            groups_html,
            "<section class='admin-group'>\
             <h2>\
               <span class='admin-group-name'>{name}</span>\
               <form method='post' action='/admin/group/remove' class='inline group-delete' \
                     onsubmit=\"return confirm('Remove group {group_js} and its feeds?');\">\
                 <input type='hidden' name='name' value='{name_attr}'>\
                 <button type='submit' class='btn-icon' aria-label='Remove group {name_attr}' title='Remove group'>\u{00d7}</button>\
               </form>\
             </h2>",
            name = name_text,
            name_attr = name_attr,
            group_js = group_js,
        )
        .unwrap();
        if g.feeds.is_empty() {
            groups_html.push_str("<p class='empty'>(no feeds)</p>");
        } else {
            groups_html.push_str("<ul class='list'>");
            for url in &g.feeds {
                let url_text = encode_text(url);
                let url_attr = encode_single_quoted_attribute(url);
                let url_js = url_text.replace('\'', "");
                write!(
                    groups_html,
                    "<li><span class='meta'>{url}</span>\
                     <form method='post' action='/admin/feed/remove' class='inline' \
                           onsubmit=\"return confirm('Remove {url_js} from this group?');\">\
                       <input type='hidden' name='group' value='{group}'>\
                       <input type='hidden' name='url' value='{url_attr}'>\
                       <button type='submit' class='btn-icon' aria-label='Remove feed {url_attr}' title='Remove feed'>\u{00d7}</button>\
                     </form></li>",
                    url = url_text,
                    url_js = url_js,
                    url_attr = url_attr,
                    group = name_attr,
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
               <button type='submit' class='btn-compact'>Add feed</button>\
             </form>\
             </section>",
            group = name_attr,
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

async fn admin_add_feed(State(state): State<Arc<AppState>>, Form(f): Form<FeedForm>) -> Redirect {
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

async fn admin_add_group(State(state): State<Arc<AppState>>, Form(f): Form<GroupForm>) -> Redirect {
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

/// Upper bound on the OPML file we'll parse. A typical reader's export
/// is well under 100 KiB; 1 MiB is generous. Enforced in the handler
/// because axum's `DefaultBodyLimit` does not apply to multipart bodies.
const OPML_MAX_BYTES: usize = 1024 * 1024;

async fn admin_import_opml(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Redirect {
    let mut xml: Option<String> = None;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                if field.name() != Some("opml") {
                    continue;
                }
                let bytes = match field.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        return redirect_with_flash(
                            None,
                            Some(&format!("OPML upload failed: {}", e)),
                        );
                    }
                };
                if bytes.is_empty() {
                    return redirect_with_flash(None, Some("OPML file is empty."));
                }
                if bytes.len() > OPML_MAX_BYTES {
                    return redirect_with_flash(None, Some("OPML file too large (max 1 MiB)."));
                }
                xml = match std::str::from_utf8(&bytes) {
                    Ok(s) => Some(s.to_string()),
                    Err(_) => {
                        return redirect_with_flash(None, Some("OPML file is not valid UTF-8."));
                    }
                };
                break;
            }
            Ok(None) => break,
            Err(e) => {
                return redirect_with_flash(None, Some(&format!("OPML upload failed: {}", e)));
            }
        }
    }
    let Some(xml) = xml else {
        return redirect_with_flash(None, Some("No OPML file selected."));
    };

    match admin::import_opml(&state, &xml).await {
        Ok(s) => {
            let msg = format!(
                "Imported {feeds} feed(s) into {groups} new group(s); \
                 {dup} duplicate(s), {bad} invalid skipped.",
                feeds = s.feeds_added,
                groups = s.groups_created,
                dup = s.skipped_duplicate,
                bad = s.skipped_invalid,
            );
            redirect_with_flash(Some(&msg), None)
        }
        Err(e) => redirect_with_flash(None, Some(&format!("OPML import failed: {}", e))),
    }
}

#[derive(Deserialize)]
struct FeedSearchQ {
    #[serde(default)]
    q: String,
}

/// Proxy the admin autocomplete query to every configured discovery
/// provider, then return a flat JSON array. Errors are deliberately
/// swallowed in [`feed_search::search_all`] so a flaky provider can't
/// take the autocomplete down.
async fn admin_feed_search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FeedSearchQ>,
) -> Json<Vec<feed_search::SearchResult>> {
    // discovery_http is the redirect-disabled client; feed_search
    // re-checks each hop's host against the SSRF block-list.
    Json(feed_search::search_all(&state.discovery_http, &state.feed_search, &q.q).await)
}

// ---------------------------------------------------------------------------
// /img — Kindle-optimized image proxy
//
// Article HTML is rewritten in `sanitize_images` so every <img> points
// here. The first hit on a given URL triggers a fetch + transcode (see
// `crate::img`); subsequent hits are served from the SQLite image
// cache. On any error — SSRF reject, source 404, undecodable bytes —
// the proxy returns 404 so the Kindle renders the <img>'s alt text.

#[derive(Deserialize)]
struct ImgQ {
    /// Source image URL — must already be percent-encoded by whoever
    /// built the proxy link (sanitize_images does this).
    #[serde(default)]
    u: String,
}

async fn image_proxy(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ImgQ>,
) -> axum::response::Response {
    if q.u.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing u").into_response();
    }
    let hash = img::hash_url(&q.u);

    // Fast path: cache hit.
    if let Some(bytes) = read_cached_image(&state, &hash).await {
        return jpeg_response(bytes);
    }

    // Cache miss: fetch and transcode. Any failure returns 404 so the
    // browser falls back to the alt text — the article reads through
    // even when an image source is gone or unsupported.
    let jpeg = match img::fetch_and_transcode(&state.http, &q.u).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("img proxy: transcode failed for {}: {}", q.u, e);
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    if let Err(e) = store_cached_image(&state, &hash, &jpeg).await {
        // Cache-write failure isn't user-visible — we still have the
        // bytes, so log and serve them.
        tracing::warn!("img proxy: cache write failed for {}: {}", q.u, e);
    }
    jpeg_response(jpeg)
}

async fn read_cached_image(state: &AppState, hash: &str) -> Option<Vec<u8>> {
    let conn = state.db.lock().await;
    conn.query_row(
        "SELECT jpeg FROM image_cache WHERE hash = ?1",
        [hash],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .ok()
}

async fn store_cached_image(state: &AppState, hash: &str, jpeg: &[u8]) -> anyhow::Result<()> {
    let conn = state.db.lock().await;
    conn.execute(
        "INSERT OR REPLACE INTO image_cache (hash, jpeg, fetched_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![hash, jpeg, now_secs()],
    )?;
    Ok(())
}

fn jpeg_response(bytes: Vec<u8>) -> axum::response::Response {
    // Cache-Control aggressively — the cache key is sha1(url), so any
    // change to the URL produces a new key. 30 days mirrors the
    // default article TTL.
    (
        [
            ("content-type", "image/jpeg"),
            ("cache-control", "public, max-age=2592000, immutable"),
        ],
        bytes,
    )
        .into_response()
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
        assert!(!effective_compact(
            &jar_with("compact", "1"),
            Some(0),
            false
        ));
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
        assert!(!effective_dark(
            &jar_with("theme", "dark"),
            Some("light"),
            false
        ));
        assert!(effective_dark(
            &jar_with("theme", "light"),
            Some("dark"),
            false
        ));
    }

    #[test]
    fn effective_dark_accepts_1_and_0_aliases() {
        assert!(effective_dark(&make_jar(), Some("1"), false));
        assert!(!effective_dark(&make_jar(), Some("0"), true));
    }

    // ----- route-handler integration tests --------------------------------
    //
    // Driven through the real `Router` so we catch wiring bugs (wrong
    // method, missing extractor, path-param name mismatch) on top of the
    // handler logic itself.

    use crate::admin::SCHEMA;
    use crate::state::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tokio::sync::{Mutex as TokioMutex, RwLock as TokioRwLock};
    use tower::ServiceExt;

    fn fresh_app_state() -> Arc<AppState> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Arc::new(AppState {
            feeds: TokioRwLock::new(Vec::new()),
            feed_titles: TokioRwLock::new(Vec::new()),
            groups: TokioRwLock::new(Vec::new()),
            http: reqwest::Client::new(),
            discovery_http: reqwest::Client::new(),
            feed_cache: TokioRwLock::new(std::collections::HashMap::new()),
            db: TokioMutex::new(conn),
            feed_ttl: std::time::Duration::from_secs(60),
            article_ttl_secs: 86400,
            compact_default: false,
            dark_default: false,
            feed_search: crate::config::FeedSearchConfig::default(),
        })
    }

    fn form(body: &str) -> Body {
        Body::from(body.to_string())
    }

    fn post(path: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form(body))
            .unwrap()
    }

    fn get(path: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn iid16(s: &str) -> String {
        crate::feeds::item_id(s)
    }

    #[tokio::test]
    async fn post_bookmark_redirect_lands_at_item_anchor() {
        // Scroll preservation depends on the bookmark/unbookmark
        // handlers appending `#item-{iid}` to the back URL — without
        // it, the browser scrolls to the top of the listing on every
        // toggle. The matching id='item-{iid}' lives on each <li>
        // in render_entries.
        let state = fresh_app_state();
        let iid = iid16("https://example.com/article");
        let resp = router(state.clone())
            .oneshot(post(
                &format!("/bookmark/{}", iid),
                "url=https%3A%2F%2Fexample.com%2Farticle&title=Hello&from=%2F%3Fpage%3D2",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(loc, format!("/?page=2#item-{}", iid));
    }

    #[tokio::test]
    async fn post_unbookmark_redirect_lands_at_item_anchor() {
        let state = fresh_app_state();
        let iid = iid16("https://example.com/article");
        let resp = router(state.clone())
            .oneshot(post(&format!("/unbookmark/{}", iid), "from=%2Fread-later"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(loc, format!("/read-later#item-{}", iid));
    }

    #[tokio::test]
    async fn post_bookmark_anchor_drops_preexisting_fragment_on_from() {
        // If a client somehow posts a `from` with its own fragment,
        // we must not produce a URL with two `#`. Last anchor wins;
        // the row anchor is the only one that matters for UX here.
        let state = fresh_app_state();
        let iid = iid16("https://example.com/article");
        let resp = router(state.clone())
            .oneshot(post(
                &format!("/bookmark/{}", iid),
                // from=/?page=1#item-other (url-encoded)
                "url=https%3A%2F%2Fexample.com%2Farticle&title=Hello&from=%2F%3Fpage%3D1%23item-other",
            ))
            .await
            .unwrap();
        let loc = resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(loc.matches('#').count(), 1);
        assert!(loc.ends_with(&format!("#item-{}", iid)));
    }

    #[tokio::test]
    async fn post_bookmark_then_get_read_later_lists_the_entry() {
        let state = fresh_app_state();
        let iid = iid16("https://example.com/article");
        let resp = router(state.clone())
            .oneshot(post(
                &format!("/bookmark/{}", iid),
                "url=https%3A%2F%2Fexample.com%2Farticle&title=Hello&from=%2F",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);

        let resp = router(state.clone())
            .oneshot(get("/read-later"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("Hello"));
        assert!(html.contains("example.com"));
        assert!(html.contains(&format!("/unbookmark/{}", iid)));
    }

    #[tokio::test]
    async fn post_bookmark_rejects_non_hex_iid_with_400() {
        let state = fresh_app_state();
        let resp = router(state.clone())
            .oneshot(post(
                "/bookmark/not-a-hex-id",
                "url=https%3A%2F%2Fx.com&title=t&from=%2F",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // No row should have landed in the DB.
        let conn = state.db.lock().await;
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM bookmark", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn post_bookmark_silently_drops_javascript_url() {
        // Validates the scheme guard wired through `validate_feed_url`.
        let state = fresh_app_state();
        let iid = iid16("https://example.com/article");
        let resp = router(state.clone())
            .oneshot(post(
                &format!("/bookmark/{}", iid),
                "url=javascript%3Aalert(1)&title=Bad&from=%2F",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let conn = state.db.lock().await;
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM bookmark", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "javascript: URL must not persist");
    }

    #[tokio::test]
    async fn post_bookmark_silently_drops_missing_title_or_url() {
        let state = fresh_app_state();
        let iid = iid16("https://example.com/article");
        // Empty title.
        let resp = router(state.clone())
            .oneshot(post(
                &format!("/bookmark/{}", iid),
                "url=https%3A%2F%2Fx.com&title=&from=%2F",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        // Empty url.
        let resp = router(state.clone())
            .oneshot(post(
                &format!("/bookmark/{}", iid),
                "url=&title=Hello&from=%2F",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let conn = state.db.lock().await;
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM bookmark", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn post_unbookmark_removes_existing_row_and_silently_no_ops_otherwise() {
        let state = fresh_app_state();
        let iid = iid16("https://example.com/article");
        // First add.
        router(state.clone())
            .oneshot(post(
                &format!("/bookmark/{}", iid),
                "url=https%3A%2F%2Fexample.com%2Farticle&title=Hello&from=%2F",
            ))
            .await
            .unwrap();
        // Then remove.
        let resp = router(state.clone())
            .oneshot(post(&format!("/unbookmark/{}", iid), "from=%2F"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        {
            let conn = state.db.lock().await;
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM bookmark", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0);
        }
        // Removing again is a no-op (still 303, no error).
        let resp = router(state.clone())
            .oneshot(post(&format!("/unbookmark/{}", iid), "from=%2F"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn post_unbookmark_rejects_non_hex_iid_with_400() {
        let state = fresh_app_state();
        let resp = router(state.clone())
            .oneshot(post("/unbookmark/totally-bogus", "from=%2F"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn read_later_empty_state_when_no_bookmarks() {
        let state = fresh_app_state();
        let resp = router(state.clone())
            .oneshot(get("/read-later"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(html.contains("No bookmarks yet"));
    }

    #[tokio::test]
    async fn admin_feed_search_empty_query_returns_empty_array() {
        // Doesn't touch the network — empty/short-circuit path in
        // feed_search::search_all.
        let state = fresh_app_state();
        let resp = router(state.clone())
            .oneshot(get("/admin/feed-search?q="))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "[]");
    }

    #[tokio::test]
    async fn admin_index_renders_toolbar_before_group_sections() {
        // Issue #23: the add-group and import-OPML forms now live in a
        // top-of-page toolbar instead of taking up two full-width
        // sections at the bottom. Pin the order (toolbar first, then
        // group sections) so a future template edit doesn't quietly
        // move them back.
        let state = fresh_app_state();
        // Seed a group so a section is rendered too.
        router(state.clone())
            .oneshot(post("/admin/group/add", "name=Tech"))
            .await
            .unwrap();
        let resp = router(state.clone()).oneshot(get("/admin")).await.unwrap();
        let html = body_string(resp).await;

        let toolbar_pos = html
            .find("class=\"admin-toolbar\"")
            .expect("admin-toolbar present");
        let group_pos = html
            .find("class='admin-group'")
            .expect("admin-group section present");
        assert!(
            toolbar_pos < group_pos,
            "toolbar must render above per-group sections"
        );
        // Toolbar carries both forms.
        assert!(html.contains("action=\"/admin/group/add\""));
        assert!(html.contains("action=\"/admin/import-opml\""));
    }

    #[tokio::test]
    async fn admin_index_renders_inline_delete_icons_on_groups_and_feeds() {
        // The remove-group / remove-feed full-text buttons are gone;
        // both are now small × icons (.btn-icon) so the group section
        // reads as a list, not a row of heavy controls.
        let state = fresh_app_state();
        router(state.clone())
            .oneshot(post("/admin/group/add", "name=Tech"))
            .await
            .unwrap();
        router(state.clone())
            .oneshot(post(
                "/admin/feed/add",
                "group=Tech&url=https%3A%2F%2Fa.example%2Frss",
            ))
            .await
            .unwrap();
        let resp = router(state.clone()).oneshot(get("/admin")).await.unwrap();
        let html = body_string(resp).await;

        // No `Remove`/`Remove group` text buttons left over.
        assert!(!html.contains(">Remove group<"), "found legacy text button");
        assert!(!html.contains(">Remove<"), "found legacy text button");
        // Inline icon buttons present, scoped to the right actions.
        assert!(html.contains("action='/admin/group/remove'"));
        assert!(html.contains("action='/admin/feed/remove'"));
        assert!(html.contains("class='btn-icon'"));
        // The × glyph is U+00D7.
        assert!(html.contains("\u{00d7}"));
        // Feed removal now also confirms first (used to skip the prompt).
        assert!(html.contains("Remove https://a.example/rss from this group?"));
    }

    #[tokio::test]
    async fn admin_add_group_and_list_through_http() {
        // Whole-flow regression: form post → admin handler → DB ops →
        // apply_from_db → next request sees the new group rendered.
        let state = fresh_app_state();
        let resp = router(state.clone())
            .oneshot(post("/admin/group/add", "name=Tech"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let resp = router(state.clone()).oneshot(get("/admin")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_string(resp).await;
        assert!(
            html.contains(">Tech<"),
            "expected group heading, got: {}",
            html
        );
    }

    // ----- OPML import route tests ----------------------------------------

    const MP_BOUNDARY: &str = "----InkwellOpmlTestBoundary";

    /// Build a multipart/form-data body containing a single `opml` part.
    /// `field_name` parameterizes the form field so we can also test the
    /// "no opml field" rejection branch.
    fn multipart_body(field_name: &str, file_contents: &[u8]) -> Vec<u8> {
        let header = format!(
            "--{b}\r\n\
             Content-Disposition: form-data; name=\"{name}\"; filename=\"feeds.opml\"\r\n\
             Content-Type: application/xml\r\n\r\n",
            b = MP_BOUNDARY,
            name = field_name,
        );
        let trailer = format!("\r\n--{}--\r\n", MP_BOUNDARY);
        let mut out = Vec::with_capacity(header.len() + file_contents.len() + trailer.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(file_contents);
        out.extend_from_slice(trailer.as_bytes());
        out
    }

    fn post_multipart(path: &str, body: Vec<u8>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={}", MP_BOUNDARY),
            )
            .body(Body::from(body))
            .unwrap()
    }

    fn opml_doc(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <opml version="2.0"><head><title>x</title></head><body>{body}</body></opml>"#
        )
    }

    fn location_of(resp: &axum::response::Response) -> String {
        resp.headers()
            .get("location")
            .expect("redirect missing Location header")
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn import_opml_endpoint_imports_feeds_and_redirects_with_ok_flash() {
        let state = fresh_app_state();
        let xml = opml_doc(
            r#"<outline text="Tech">
                <outline xmlUrl="https://a.example/rss"/>
              </outline>"#,
        );
        let req = post_multipart("/admin/import-opml", multipart_body("opml", xml.as_bytes()));
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = location_of(&resp);
        assert!(loc.starts_with("/admin?ok="), "got Location: {}", loc);
        // And the new feed shows up on the admin page.
        let resp = router(state.clone()).oneshot(get("/admin")).await.unwrap();
        let html = body_string(resp).await;
        assert!(html.contains("https://a.example/rss"));
        assert!(html.contains(">Tech<"));
    }

    #[tokio::test]
    async fn import_opml_endpoint_creates_uncategorized_group_for_loose_feeds() {
        let state = fresh_app_state();
        let xml = opml_doc(r#"<outline xmlUrl="https://loose.example/rss"/>"#);
        let req = post_multipart("/admin/import-opml", multipart_body("opml", xml.as_bytes()));
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let resp = router(state.clone()).oneshot(get("/admin")).await.unwrap();
        let html = body_string(resp).await;
        assert!(html.contains(">Uncategorized<"));
    }

    #[tokio::test]
    async fn import_opml_endpoint_missing_file_field_redirects_with_err_flash() {
        let state = fresh_app_state();
        // Field name is "wrong-name" so the handler never finds the
        // opml part — should redirect with err, not 500.
        let req = post_multipart(
            "/admin/import-opml",
            multipart_body("wrong-name", b"<opml/>"),
        );
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = location_of(&resp);
        assert!(loc.starts_with("/admin?err="), "got Location: {}", loc);
        assert!(state.feeds.read().await.is_empty());
    }

    #[tokio::test]
    async fn import_opml_endpoint_empty_file_redirects_with_err_flash() {
        let state = fresh_app_state();
        let req = post_multipart("/admin/import-opml", multipart_body("opml", b""));
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = location_of(&resp);
        assert!(loc.starts_with("/admin?err="), "got Location: {}", loc);
    }

    #[tokio::test]
    async fn import_opml_endpoint_malformed_xml_redirects_with_err_flash() {
        let state = fresh_app_state();
        let bad = br#"<?xml version="1.0"?><opml><body><outline xmlUrl="x"></body></opml>"#;
        let req = post_multipart("/admin/import-opml", multipart_body("opml", bad));
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = location_of(&resp);
        assert!(loc.starts_with("/admin?err="), "got Location: {}", loc);
        assert!(state.feeds.read().await.is_empty());
    }

    #[tokio::test]
    async fn import_opml_endpoint_oversize_body_redirects_with_err_flash() {
        // 1.5 MiB payload: above the handler's 1 MiB OPML cap, below
        // axum's 2 MiB router-default body limit so we exercise the
        // handler's own check (and not the framework's truncation
        // error path, which produces a less helpful flash).
        let state = fresh_app_state();
        let big = vec![b'A'; (3 * 1024 * 1024) / 2];
        let req = post_multipart("/admin/import-opml", multipart_body("opml", &big));
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = location_of(&resp);
        assert!(loc.starts_with("/admin?err="), "got Location: {}", loc);
        assert!(
            loc.contains("too") && loc.contains("large"),
            "expected 'too large' in flash, got: {}",
            loc
        );
        assert!(state.feeds.read().await.is_empty());
    }

    #[tokio::test]
    async fn import_opml_endpoint_rejects_unsafe_schemes_silently() {
        // The handler still redirects with `ok` (the import succeeded —
        // zero feeds added, three skipped), but no row hits the DB.
        let state = fresh_app_state();
        let xml = opml_doc(
            r#"<outline text="Bad">
                <outline xmlUrl="javascript:alert(1)"/>
                <outline xmlUrl="file:///etc/passwd"/>
                <outline xmlUrl="ftp://example.com/feed"/>
               </outline>"#,
        );
        let req = post_multipart("/admin/import-opml", multipart_body("opml", xml.as_bytes()));
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        // Group was created (it had outlines) but no feeds landed.
        assert!(state.feeds.read().await.is_empty());
    }

    // ----- /img proxy tests (issue #21) -----------------------------------

    async fn seed_image_cache(state: &Arc<AppState>, hash: &str, jpeg: &[u8], fetched_at: i64) {
        let conn = state.db.lock().await;
        let bytes = jpeg.to_vec();
        conn.execute(
            "INSERT INTO image_cache (hash, jpeg, fetched_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![hash, bytes, fetched_at],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn img_proxy_serves_cached_jpeg() {
        let state = fresh_app_state();
        let url = "https://example.com/photo.png";
        // The "JPEG" payload is arbitrary — the handler returns the
        // bytes verbatim from the cache, no re-validation.
        let payload = vec![0xff, 0xd8, 0xff, 0xe0, 0, 16, b'J', b'F', b'I', b'F'];
        seed_image_cache(&state, &crate::img::hash_url(url), &payload, 1).await;

        let req = get(&format!("/img?u={}", url_encode(url)));
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-type").unwrap(), "image/jpeg");
        assert!(
            resp.headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("max-age="),
            "long Cache-Control missing"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), payload.as_slice());
    }

    #[tokio::test]
    async fn img_proxy_returns_400_when_u_is_missing() {
        let state = fresh_app_state();
        let resp = router(state.clone()).oneshot(get("/img")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn img_proxy_returns_400_when_u_is_empty() {
        let state = fresh_app_state();
        let resp = router(state.clone()).oneshot(get("/img?u=")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn img_proxy_rejects_non_http_scheme_with_404() {
        // SSRF guard at the URL-validation layer; the handler should
        // never reach a network fetch.
        let state = fresh_app_state();
        let resp = router(state.clone())
            .oneshot(get("/img?u=file%3A%2F%2F%2Fetc%2Fpasswd"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn img_proxy_rejects_loopback_host_with_404() {
        let state = fresh_app_state();
        let resp = router(state.clone())
            .oneshot(get("/img?u=http%3A%2F%2F127.0.0.1%2Fphoto.png"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
