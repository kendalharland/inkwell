//! HTML rendering for Kindle: page wrapper, listings, pagination.
//!
//! Markup lives in `src/templates/*.html` and is `include_str!`'d into the
//! binary at compile time, then expanded with [`crate::template::render`]
//! (a tiny single-pass `{{var}}` substituter — no template engine).
//!
//! Keeping the markup in dedicated files makes it scannable at a glance
//! and lets editor HTML tooling work on it. The `render` helper does a
//! single substitution pass so a value that itself contains `{{...}}`
//! cannot trigger nested expansion.

use std::{
    collections::HashSet,
    fmt::Write as _,
    time::{SystemTime, UNIX_EPOCH},
};

use html_escape::{encode_single_quoted_attribute, encode_text};

use crate::{feeds::EntryView, template::render};

pub const PAGE_SIZE: usize = 25;

pub const STYLE: &str = include_str!("templates/style.css");
const PAGE_TEMPLATE: &str = include_str!("templates/page.html");
const LISTING_TEMPLATE: &str = include_str!("templates/listing.html");

pub fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Small bookmark form rendered alongside each listing entry. Two
/// hidden inputs carry the url and title so the bookmark row can stand
/// on its own after the feed has rolled the entry off. `from` rides
/// back into the redirect so the user lands on the same page+pagination
/// they were viewing.
///
/// All attribute interpolations use `encode_single_quoted_attribute`
/// because the form's attributes are single-quoted — plain `encode_text`
/// leaves `'` unescaped and a feed title containing one (e.g.
/// "What's new") would close the attribute and corrupt the row (#17).
pub fn render_bookmark_button(e: &EntryView, from_path: &str, bookmarked: bool) -> String {
    let (action, glyph, label) = if bookmarked {
        ("/unbookmark", "★", "Remove bookmark")
    } else {
        ("/bookmark", "☆", "Save for later")
    };
    format!(
        "<form method='post' action='{action}/{iid}' class='bookmark-form'>\
         <input type='hidden' name='url' value='{url}'>\
         <input type='hidden' name='title' value='{title}'>\
         <input type='hidden' name='from' value='{from}'>\
         <button type='submit' class='bookmark-btn' aria-label='{label}'>{glyph}</button>\
         </form>",
        action = action,
        iid = e.iid,
        url = encode_single_quoted_attribute(&e.url),
        title = encode_single_quoted_attribute(&e.title),
        from = encode_single_quoted_attribute(from_path),
        label = label,
        glyph = glyph,
    )
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Outer chrome — `<head>`, top nav, body wrapper. Every route returns
/// this around its content. `body_class` is interpolated into the
/// `<body class=...>` attribute so layout-density variants (compact mode)
/// can be toggled without re-rendering everything.
pub fn page(title: &str, body: &str, body_class: &str) -> String {
    render(
        PAGE_TEMPLATE,
        &[
            ("title", &encode_text(title)),
            ("style", STYLE),
            ("body_class", body_class),
            ("body", body),
        ],
    )
}

/// Render a paginated listing of entries with a back-link `from` query
/// param so the article page can offer a working Back button.
///
/// `show_source = true` appends the feed title to each item; useful in
/// merged listings ("All stories", group views), redundant in
/// single-feed views.
pub fn render_entries(
    title: &str,
    entries: &[EntryView],
    bookmarks: &HashSet<String>,
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

    if entries.is_empty() {
        return render(
            LISTING_TEMPLATE,
            &[
                ("title", &encode_text(title)),
                ("items", "<div class='empty'>No items.</div>"),
                ("pager", ""),
            ],
        );
    }

    let mut items = String::from("<ul class='list'>");
    let from = format!("{}?page={}", base_path, page_num);
    let from_enc = url_encode(&from);
    for e in &entries[start..end] {
        let source = if show_source {
            format!(" — {}", encode_text(&e.feed_title))
        } else {
            String::new()
        };
        let bookmarked = bookmarks.contains(&e.iid);
        let bm = render_bookmark_button(e, &from, bookmarked);
        write!(
            items,
            "<li class='entry'>{bm}\
             <div class='entry-body'>\
             <a href='/item/{iid}?from={from}'>{title}</a>\
             <div class='meta'>{host}{source}</div>\
             </div></li>",
            bm = bm,
            iid = e.iid,
            from = from_enc,
            title = encode_text(&e.title),
            host = encode_text(&e.host),
            source = source,
        )
        .unwrap();
    }
    items.push_str("</ul>");

    let pager = if total_pages > 1 {
        let mut p = String::from("<div class='pager'>");
        if page_num > 1 {
            write!(p, "<a href='{}?page={}'>Previous</a>", base_path, page_num - 1).unwrap();
        }
        write!(p, "<span>Page {} of {}</span>", page_num, total_pages).unwrap();
        if page_num < total_pages {
            write!(p, "<a href='{}?page={}'>Next</a>", base_path, page_num + 1).unwrap();
        }
        p.push_str("</div>");
        p
    } else {
        String::new()
    };

    render(
        LISTING_TEMPLATE,
        &[
            ("title", &encode_text(title)),
            ("items", &items),
            ("pager", &pager),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(iid: &str, title: &str, ts: i64) -> EntryView {
        EntryView {
            iid: iid.into(),
            title: title.into(),
            host: "example.com".into(),
            url: "https://example.com/x".into(),
            published_ts: ts,
            feed_title: "Test Feed".into(),
        }
    }

    #[test]
    fn url_encode_basic() {
        assert_eq!(url_encode("/feed/0?page=2"), "%2Ffeed%2F0%3Fpage%3D2");
        assert_eq!(url_encode("simple"), "simple");
    }

    #[test]
    fn page_includes_top_nav() {
        let p = page("X", "<p>hi</p>", "");
        assert!(p.contains("All stories"));
        assert!(p.contains("/feeds"));
        assert!(p.contains("/groups"));
        assert!(p.contains("<p>hi</p>"));
    }

    #[test]
    fn page_escapes_title() {
        let p = page("<script>", "", "");
        assert!(!p.contains("<title><script>"));
        assert!(p.contains("&lt;script&gt;"));
    }

    #[test]
    fn page_passes_body_class_through() {
        let p = page("X", "", "compact");
        assert!(p.contains(r#"<body class="compact""#));
    }

    fn no_bookmarks() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn render_entries_shows_empty_state() {
        let out = render_entries("None", &[], &no_bookmarks(), 1, "/feed/0", false);
        assert!(out.contains("No items."));
        assert!(!out.contains("class='pager'"));
    }

    #[test]
    fn render_entries_no_pager_on_single_page() {
        let entries: Vec<_> = (0..PAGE_SIZE)
            .map(|i| ev(&format!("{:016x}", i), &format!("t{}", i), i as i64))
            .collect();
        let out = render_entries("All", &entries, &no_bookmarks(), 1, "/", true);
        assert!(!out.contains("class='pager'"));
        assert!(out.contains("t0"));
        assert!(out.contains(&format!("t{}", PAGE_SIZE - 1)));
    }

    #[test]
    fn render_entries_paginates_at_page_size() {
        let entries: Vec<_> = (0..PAGE_SIZE * 2 + 5)
            .map(|i| ev(&format!("{:016x}", i), &format!("t{}", i), i as i64))
            .collect();
        let p1 = render_entries("All", &entries, &no_bookmarks(), 1, "/", true);
        let p2 = render_entries("All", &entries, &no_bookmarks(), 2, "/", true);
        let p3 = render_entries("All", &entries, &no_bookmarks(), 3, "/", true);

        assert!(p1.contains("Page 1 of 3"));
        assert!(p1.contains(">Next<"));
        assert!(!p1.contains(">Previous<"));

        assert!(p2.contains("Page 2 of 3"));
        assert!(p2.contains(">Previous<"));
        assert!(p2.contains(">Next<"));

        assert!(p3.contains("Page 3 of 3"));
        assert!(p3.contains(">Previous<"));
        assert!(!p3.contains(">Next<"));
    }

    #[test]
    fn render_entries_clamps_oversize_page() {
        let entries: Vec<_> = (0..3)
            .map(|i| ev(&format!("{:016x}", i), &format!("t{}", i), i as i64))
            .collect();
        let out = render_entries("All", &entries, &no_bookmarks(), 99, "/", false);
        assert!(out.contains("t0"));
        assert!(out.contains("t2"));
    }

    #[test]
    fn show_source_toggle_emits_feed_title() {
        let entries = vec![ev("aaaa", "hello", 1)];
        let with = render_entries("X", &entries, &no_bookmarks(), 1, "/", true);
        let without = render_entries("X", &entries, &no_bookmarks(), 1, "/", false);
        assert!(with.contains("Test Feed"));
        assert!(!without.contains("Test Feed"));
    }

    #[test]
    fn render_entries_marks_bookmarked_with_filled_star() {
        let entries = vec![ev("aaaa", "hello", 1)];
        let mut bm = HashSet::new();
        bm.insert("aaaa".to_string());
        let out = render_entries("X", &entries, &bm, 1, "/", false);
        assert!(out.contains("★"), "expected filled star, got: {}", out);
        assert!(out.contains("action='/unbookmark/aaaa'"));
        assert!(!out.contains("action='/bookmark/aaaa'"));
    }

    #[test]
    fn render_entries_marks_unbookmarked_with_outline_star() {
        let entries = vec![ev("aaaa", "hello", 1)];
        let out = render_entries("X", &entries, &no_bookmarks(), 1, "/", false);
        assert!(out.contains("☆"), "expected outline star, got: {}", out);
        assert!(out.contains("action='/bookmark/aaaa'"));
        assert!(!out.contains("action='/unbookmark/aaaa'"));
    }

    #[test]
    fn render_bookmark_button_escapes_apostrophe_in_title_and_url() {
        // Regression for #17: plain encode_text leaves `'` alone, so
        // an apostrophe in a feed title would close the single-quoted
        // value attribute, truncate the stored title, and create an
        // attribute-injection sink. The fix uses
        // encode_single_quoted_attribute, which escapes `'` to its
        // hex entity.
        let e = EntryView {
            iid: "deadbeefdeadbeef".into(),
            title: "What's new ' onfocus='alert(1)".into(),
            host: "x".into(),
            url: "https://example.com/?q=it's".into(),
            published_ts: 0,
            feed_title: "F".into(),
        };
        let out = render_bookmark_button(&e, "/", false);
        // No bare apostrophe should appear inside any value=... range —
        // every `'` between two attribute quotes must be the entity.
        for v in [
            "value='https://example.com/?q=it&#x27;s'",
            "value='What&#x27;s new &#x27; onfocus=&#x27;alert(1)'",
        ] {
            assert!(out.contains(v), "missing expected escaped attr {}\nin: {}", v, out);
        }
        // The form structure must still be intact: every `value=` must
        // immediately precede a single quote (not a bare apostrophe
        // from the title leaking in).
        assert_eq!(out.matches("value='").count(), 3, "form attrs torn: {}", out);
    }
}
