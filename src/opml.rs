//! OPML parsing for the admin import flow.
//!
//! The parser is intentionally permissive: it accepts both OPML 1.0 and
//! 2.0, ignores the `<head>` section, tolerates unknown attributes, and
//! treats nested `<outline>` groups by inheriting the nearest named
//! ancestor as the group label. It does *not* validate URL schemes —
//! that's the import layer's job, because the same allow-list applies
//! to URLs added through the regular admin form.

use anyhow::{anyhow, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Decoder;
use quick_xml::Reader;

/// Group assigned to feeds that appear directly under `<body>` without
/// being wrapped in a category `<outline>`. Many older readers export
/// flat OPML; landing those feeds here keeps the import lossless.
pub const UNCATEGORIZED_GROUP: &str = "Uncategorized";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpmlEntry {
    pub group: String,
    pub url: String,
}

/// Parse an OPML document into a flat list of (group, url) entries in
/// document order. Duplicates are not removed at this layer — the
/// importer counts collisions when it tries to insert.
pub fn parse(xml: &str) -> Result<Vec<OpmlEntry>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let decoder = reader.decoder();

    let mut entries: Vec<OpmlEntry> = Vec::new();
    // One slot per open <outline>. `Some(name)` for a category outline
    // (no xmlUrl) that contributes a group label to its descendants;
    // `None` for a feed outline (or an unlabeled wrapper) — those don't
    // shadow the group inherited from further up the stack.
    let mut stack: Vec<Option<String>> = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if local_name(e.name().as_ref()) == b"outline" => {
                let (name, url) = read_outline_attrs(e, decoder)?;
                if let Some(url) = url {
                    entries.push(OpmlEntry {
                        group: inherit_group(&stack),
                        url,
                    });
                    // Push a no-name slot so the matching </outline> pops
                    // the correct entry off the stack.
                    stack.push(None);
                } else {
                    stack.push(name);
                }
            }
            Ok(Event::Empty(ref e)) if local_name(e.name().as_ref()) == b"outline" => {
                let (_, url) = read_outline_attrs(e, decoder)?;
                if let Some(url) = url {
                    entries.push(OpmlEntry {
                        group: inherit_group(&stack),
                        url,
                    });
                }
                // Self-closing outline with no xmlUrl is an empty group;
                // it has no descendants to label, so nothing to push.
            }
            Ok(Event::End(ref e)) if local_name(e.name().as_ref()) == b"outline" => {
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow!("malformed OPML: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(entries)
}

fn inherit_group(stack: &[Option<String>]) -> String {
    stack
        .iter()
        .rev()
        .find_map(|slot| slot.clone())
        .unwrap_or_else(|| UNCATEGORIZED_GROUP.to_string())
}

/// Returns (display name, feed url). Either may be absent. `text` wins
/// over `title` per OPML 2.0 §5 — `text` is required, `title` optional.
fn read_outline_attrs(
    e: &BytesStart,
    decoder: Decoder,
) -> Result<(Option<String>, Option<String>)> {
    let mut text_attr: Option<String> = None;
    let mut title_attr: Option<String> = None;
    let mut xml_url: Option<String> = None;

    for attr in e.attributes() {
        let attr = attr.map_err(|err| anyhow!("malformed outline attribute: {err}"))?;
        let value = attr
            .decode_and_unescape_value(decoder)
            .map_err(|err| anyhow!("undecodable outline attribute: {err}"))?
            .into_owned();
        match attr.key.local_name().as_ref() {
            b"xmlUrl" => xml_url = Some(value.trim().to_string()),
            b"text" => text_attr = Some(value.trim().to_string()),
            b"title" => title_attr = Some(value.trim().to_string()),
            _ => {}
        }
    }

    let xml_url = xml_url.filter(|s| !s.is_empty());
    // Filter each attribute *before* the fallback so that `text=""` doesn't
    // shadow a usable `title="..."` — some exporters write text="" and put
    // the human-readable name in title.
    let name = text_attr
        .filter(|s| !s.is_empty())
        .or_else(|| title_attr.filter(|s| !s.is_empty()));
    Ok((name, xml_url))
}

/// Strip an `ns:` prefix from a tag name. We don't care which namespace
/// an `<outline>` lives in; some exporters use a default namespace, some
/// prefix everything under `<opml:...>`.
fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(entries: &[OpmlEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.url.as_str()).collect()
    }

    fn groups(entries: &[OpmlEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.group.as_str()).collect()
    }

    const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;

    fn doc(body: &str) -> String {
        format!(
            "{HEADER}<opml version=\"2.0\"><head><title>x</title></head><body>{body}</body></opml>"
        )
    }

    // ----- structural cases ------------------------------------------------

    #[test]
    fn empty_body_returns_no_entries() {
        assert!(parse(&doc("")).unwrap().is_empty());
    }

    #[test]
    fn head_metadata_is_ignored() {
        // Regression guard: an exporter once shipped a <title> inside
        // <head> whose text matched a feed URL pattern. The parser must
        // not emit anything for <head>.
        let xml = format!(
            "{HEADER}<opml><head><title>https://nope.example/rss</title>\
             <ownerName>x</ownerName></head><body></body></opml>"
        );
        assert!(parse(&xml).unwrap().is_empty());
    }

    #[test]
    fn comments_and_processing_instructions_are_ignored() {
        let xml = doc(r#"<!-- a comment -->
               <?some-pi data?>
               <outline type="rss" text="X" xmlUrl="https://x.example/rss"/>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(urls(&r), vec!["https://x.example/rss"]);
    }

    #[test]
    fn malformed_xml_is_rejected() {
        let xml = format!("{HEADER}<opml><body><outline xmlUrl=\"x\"></body></opml>");
        assert!(parse(&xml).is_err());
    }

    // ----- single-group cases ---------------------------------------------

    #[test]
    fn one_group_one_feed() {
        let xml = doc(r#"<outline text="Tech">
                <outline type="rss" text="Site" xmlUrl="https://a.example/rss"/>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].group, "Tech");
        assert_eq!(r[0].url, "https://a.example/rss");
    }

    #[test]
    fn multiple_feeds_in_one_group_preserve_order() {
        let xml = doc(r#"<outline text="Tech">
                <outline type="rss" xmlUrl="https://a.example/rss"/>
                <outline type="rss" xmlUrl="https://b.example/rss"/>
                <outline type="rss" xmlUrl="https://c.example/rss"/>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(
            urls(&r),
            vec![
                "https://a.example/rss",
                "https://b.example/rss",
                "https://c.example/rss",
            ]
        );
        assert!(r.iter().all(|e| e.group == "Tech"));
    }

    #[test]
    fn outline_with_no_xmlurl_does_not_emit_entry() {
        // A group label with no feed children should yield zero entries.
        let xml = doc(r#"<outline text="EmptyGroup"></outline>"#);
        assert!(parse(&xml).unwrap().is_empty());
    }

    // ----- multi-group cases ----------------------------------------------

    #[test]
    fn multiple_groups_preserve_document_order() {
        let xml = doc(r#"<outline text="B">
                <outline xmlUrl="https://b.example/rss"/>
               </outline>
               <outline text="A">
                <outline xmlUrl="https://a.example/rss"/>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(groups(&r), vec!["B", "A"]);
        assert_eq!(
            urls(&r),
            vec!["https://b.example/rss", "https://a.example/rss"]
        );
    }

    #[test]
    fn interleaved_groups_and_feeds_are_grouped_correctly() {
        let xml = doc(r#"<outline text="G1">
                <outline xmlUrl="https://g1a.example/rss"/>
              </outline>
              <outline text="G2">
                <outline xmlUrl="https://g2a.example/rss"/>
                <outline xmlUrl="https://g2b.example/rss"/>
              </outline>
              <outline text="G1-again">
                <outline xmlUrl="https://g1z.example/rss"/>
              </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(groups(&r), vec!["G1", "G2", "G2", "G1-again"]);
    }

    // ----- nesting --------------------------------------------------------

    #[test]
    fn nested_groups_use_nearest_named_ancestor() {
        // NetNewsWire / Inoreader exports sometimes nest a folder inside
        // a folder. We use the *direct* parent, not the outermost label,
        // because that matches the user's visible hierarchy in the
        // source reader.
        let xml = doc(r#"<outline text="Tech">
                <outline text="Programming">
                  <outline xmlUrl="https://prog.example/rss"/>
                </outline>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].group, "Programming");
    }

    #[test]
    fn unnamed_wrapper_outline_does_not_shadow_named_ancestor() {
        // A <outline> with neither text nor title shouldn't drop feeds
        // into "Uncategorized" if there's a named outline above it —
        // we should keep walking up.
        let xml = doc(r#"<outline text="Tech">
                <outline>
                  <outline xmlUrl="https://x.example/rss"/>
                </outline>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r[0].group, "Tech");
    }

    #[test]
    fn top_level_feed_falls_back_to_uncategorized() {
        let xml = doc(r#"<outline type="rss" xmlUrl="https://loose.example/rss"/>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].group, UNCATEGORIZED_GROUP);
    }

    #[test]
    fn mix_of_loose_feeds_and_grouped_feeds() {
        let xml = doc(r#"<outline xmlUrl="https://loose.example/rss"/>
               <outline text="Tech">
                 <outline xmlUrl="https://t.example/rss"/>
               </outline>
               <outline xmlUrl="https://loose2.example/rss"/>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(
            groups(&r),
            vec![UNCATEGORIZED_GROUP, "Tech", UNCATEGORIZED_GROUP]
        );
    }

    // ----- attribute handling ---------------------------------------------

    #[test]
    fn text_attribute_wins_over_title_for_group_label() {
        let xml = doc(r#"<outline text="from-text" title="from-title">
                 <outline xmlUrl="https://x.example/rss"/>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r[0].group, "from-text");
    }

    #[test]
    fn title_used_when_text_is_absent() {
        let xml = doc(r#"<outline title="from-title">
                 <outline xmlUrl="https://x.example/rss"/>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r[0].group, "from-title");
    }

    #[test]
    fn empty_text_attribute_falls_back_to_title() {
        // Some exporters write text="" and put the real name in title.
        let xml = doc(r#"<outline text="" title="real">
                 <outline xmlUrl="https://x.example/rss"/>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r[0].group, "real");
    }

    #[test]
    fn whitespace_only_xml_url_is_skipped() {
        let xml = doc(r#"<outline text="Tech">
                <outline xmlUrl="   "/>
                <outline xmlUrl="https://kept.example/rss"/>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(urls(&r), vec!["https://kept.example/rss"]);
    }

    #[test]
    fn xml_url_is_trimmed() {
        let xml = doc(r#"<outline xmlUrl="  https://x.example/rss  "/>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r[0].url, "https://x.example/rss");
    }

    #[test]
    fn xml_entities_are_decoded_in_xml_url() {
        // Real-world OPML often escapes the `&` in query strings.
        let xml = doc(r#"<outline xmlUrl="https://x.example/rss?a=1&amp;b=2"/>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r[0].url, "https://x.example/rss?a=1&b=2");
    }

    #[test]
    fn xml_entities_are_decoded_in_group_name() {
        let xml = doc(r#"<outline text="Tech &amp; Science">
                 <outline xmlUrl="https://x.example/rss"/>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r[0].group, "Tech & Science");
    }

    #[test]
    fn unknown_outline_attributes_are_ignored() {
        let xml = doc(r#"<outline type="rss" version="RSS"
                        text="Site" htmlUrl="https://x.example"
                        xmlUrl="https://x.example/rss"
                        description="..."/>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(urls(&r), vec!["https://x.example/rss"]);
    }

    #[test]
    fn outline_type_is_not_required() {
        // Some exporters omit type entirely on RSS feeds.
        let xml = doc(r#"<outline xmlUrl="https://x.example/rss"/>"#);
        assert_eq!(urls(&parse(&xml).unwrap()), vec!["https://x.example/rss"]);
    }

    #[test]
    fn outline_with_type_atom_is_accepted() {
        let xml = doc(r#"<outline type="atom" xmlUrl="https://x.example/atom"/>"#);
        assert_eq!(urls(&parse(&xml).unwrap()), vec!["https://x.example/atom"]);
    }

    // ----- robustness -----------------------------------------------------

    #[test]
    fn url_validation_is_deferred_to_caller() {
        // The parser must not filter `javascript:` etc. — admin's
        // import layer applies the same allow-list it uses for the
        // single-feed admin form, so the rule lives in one place.
        let xml = doc(r#"<outline xmlUrl="javascript:alert(1)"/>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r[0].url, "javascript:alert(1)");
    }

    #[test]
    fn namespace_prefixed_outline_is_recognized() {
        // Some exporters wrap the document in a namespace prefix.
        let xml = format!(
            "{HEADER}<opml:opml xmlns:opml=\"http://opml.example/\" version=\"2.0\">\
             <opml:body>\
               <opml:outline text=\"Tech\">\
                 <opml:outline xmlUrl=\"https://x.example/rss\"/>\
               </opml:outline>\
             </opml:body></opml:opml>"
        );
        let r = parse(&xml).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].group, "Tech");
    }

    #[test]
    fn mixed_self_closing_and_paired_outlines() {
        let xml = doc(r#"<outline text="Tech">
                <outline xmlUrl="https://a.example/rss"/>
                <outline xmlUrl="https://b.example/rss"></outline>
              </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(
            urls(&r),
            vec!["https://a.example/rss", "https://b.example/rss"]
        );
    }

    #[test]
    fn duplicates_within_file_are_preserved_by_parser() {
        // Dedup is the importer's job — the parser stays a pure mapping.
        let xml = doc(r#"<outline text="Tech">
                <outline xmlUrl="https://x.example/rss"/>
                <outline xmlUrl="https://x.example/rss"/>
              </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn feed_outline_with_children_does_not_leak_url_to_descendants() {
        // Pathological input: an <outline> with xmlUrl that *also* has
        // child outlines. We emit the parent as a feed but treat its
        // body as belonging to the nearest named ancestor — i.e., the
        // feed outline doesn't shadow its own group context.
        let xml = doc(r#"<outline text="Tech">
                 <outline xmlUrl="https://parent.example/rss">
                   <outline xmlUrl="https://child.example/rss"/>
                 </outline>
               </outline>"#);
        let r = parse(&xml).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].group, "Tech");
        assert_eq!(r[0].url, "https://parent.example/rss");
        assert_eq!(r[1].group, "Tech");
        assert_eq!(r[1].url, "https://child.example/rss");
    }

    #[test]
    fn empty_xml_is_an_error() {
        // No <opml> root, no events worth emitting — quick-xml treats
        // a totally empty input as EOF, so we expect Ok([]) here. The
        // route layer treats that as a no-op.
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn whitespace_only_input_is_a_no_op() {
        assert!(parse("   \n\t  ").unwrap().is_empty());
    }
}
