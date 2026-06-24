//! HTML rendering for Kindle: page wrapper, listings, pagination.
//!
//! Markup is intentionally inlined as `format!` strings rather than a
//! templating engine. The Kindle browser is unforgiving and the markup
//! surface is small enough that a template language would be more friction
//! than the variable interpolation it would save.

use std::{
    fmt::Write as _,
    time::{SystemTime, UNIX_EPOCH},
};

use html_escape::encode_text;

use crate::feeds::EntryView;

pub const PAGE_SIZE: usize = 25;

/// All inline CSS used by the reader. One file's worth, copied into every
/// response so the Kindle browser never has to do a second roundtrip just
/// for styles. Targets a font-size and column width that match the device's
/// physical screen comfortably.
pub const STYLE: &str = r#"
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

pub fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Outer chrome — `<head>`, top nav, body wrapper. Every route returns
/// this around its content.
pub fn page(title: &str, body: &str) -> String {
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

/// Render a paginated listing of entries with a back-link `from` query
/// param so the article page can offer a working Back button.
///
/// `show_source = true` appends the feed title to each item; useful in
/// merged listings ("All stories", group views), redundant in
/// single-feed views.
pub fn render_entries(
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
        write!(body, "<span>Page {} of {}</span>", page_num, total_pages).unwrap();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(iid: &str, title: &str, ts: i64) -> EntryView {
        EntryView {
            iid: iid.into(),
            title: title.into(),
            host: "example.com".into(),
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
        let p = page("X", "<p>hi</p>");
        assert!(p.contains("All stories"));
        assert!(p.contains("/feeds"));
        assert!(p.contains("/groups"));
        assert!(p.contains("<p>hi</p>"));
    }

    #[test]
    fn page_escapes_title() {
        let p = page("<script>", "");
        assert!(!p.contains("<script>"));
        assert!(p.contains("&lt;script&gt;"));
    }

    #[test]
    fn render_entries_shows_empty_state() {
        let out = render_entries("None", &[], 1, "/feed/0", false);
        assert!(out.contains("No items."));
        assert!(!out.contains("class='pager'"));
    }

    #[test]
    fn render_entries_no_pager_on_single_page() {
        let entries: Vec<_> = (0..PAGE_SIZE).map(|i| ev(&format!("{:016x}", i), &format!("t{}", i), i as i64)).collect();
        let out = render_entries("All", &entries, 1, "/", true);
        assert!(!out.contains("class='pager'"));
        assert!(out.contains("t0"));
        assert!(out.contains(&format!("t{}", PAGE_SIZE - 1)));
    }

    #[test]
    fn render_entries_paginates_at_page_size() {
        let entries: Vec<_> = (0..PAGE_SIZE * 2 + 5)
            .map(|i| ev(&format!("{:016x}", i), &format!("t{}", i), i as i64))
            .collect();
        let p1 = render_entries("All", &entries, 1, "/", true);
        let p2 = render_entries("All", &entries, 2, "/", true);
        let p3 = render_entries("All", &entries, 3, "/", true);

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
        let entries: Vec<_> = (0..3).map(|i| ev(&format!("{:016x}", i), &format!("t{}", i), i as i64)).collect();
        // Asking for page 99 of a 1-page list should render page 1.
        let out = render_entries("All", &entries, 99, "/", false);
        assert!(out.contains("t0"));
        assert!(out.contains("t2"));
    }

    #[test]
    fn show_source_toggle_emits_feed_title() {
        let entries = vec![ev("aaaa", "hello", 1)];
        let with = render_entries("X", &entries, 1, "/", true);
        let without = render_entries("X", &entries, 1, "/", false);
        assert!(with.contains("Test Feed"));
        assert!(!without.contains("Test Feed"));
    }
}
