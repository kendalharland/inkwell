//! "Read later" — per-article bookmarks persisted in SQLite.
//!
//! Each row is `(article_id, url, title, bookmarked_at)`. We carry the
//! url + title directly instead of joining `article` so a bookmarked
//! entry survives the article cache being purged and stays renderable
//! from the listing view alone.
//!
//! The article-purge job is paired with this table — it deletes from
//! `article` only where the id is NOT in `bookmark`, so a bookmarked
//! entry's body keeps living past the configured TTL.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{params, Connection};

#[derive(Debug, Clone)]
pub struct Bookmark {
    pub article_id: String,
    pub url: String,
    pub title: String,
    /// Unix seconds when the bookmark was added. Used by SQL `ORDER BY`
    /// to put newest bookmarks first; not displayed yet, but read by
    /// the list() consumer so the field has to round-trip.
    #[allow(dead_code)]
    pub bookmarked_at: i64,
}

/// Insert or refresh a bookmark. `INSERT OR REPLACE` so re-bookmarking
/// updates the stored title/url to the latest values if they've
/// changed in the feed.
pub fn add(
    conn: &Connection,
    article_id: &str,
    url: &str,
    title: &str,
    now_secs: i64,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO bookmark (article_id, url, title, bookmarked_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![article_id, url, title, now_secs],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, article_id: &str) -> Result<()> {
    conn.execute("DELETE FROM bookmark WHERE article_id = ?1", [article_id])?;
    Ok(())
}

pub fn is_bookmarked(conn: &Connection, article_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM bookmark WHERE article_id = ?1",
        [article_id],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

/// Bulk-load every bookmarked id so a listing render can mark its stars
/// in one query instead of one-per-entry. Returns an empty set on
/// error — bookmark display is non-essential and shouldn't fail a page.
pub fn load_ids(conn: &Connection) -> HashSet<String> {
    let Ok(mut stmt) = conn.prepare("SELECT article_id FROM bookmark") else {
        return HashSet::new();
    };
    stmt.query_map([], |r| r.get::<_, String>(0))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// Read-later listing — newest bookmark first.
pub fn list(conn: &Connection) -> Result<Vec<Bookmark>> {
    let rows: Vec<Bookmark> = conn
        .prepare(
            "SELECT article_id, url, title, bookmarked_at \
             FROM bookmark ORDER BY bookmarked_at DESC",
        )?
        .query_map([], |r| {
            Ok(Bookmark {
                article_id: r.get(0)?,
                url: r.get(1)?,
                title: r.get(2)?,
                bookmarked_at: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::SCHEMA;

    fn fresh_conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    #[test]
    fn add_then_check_then_remove() {
        let c = fresh_conn();
        assert!(!is_bookmarked(&c, "abc"));
        add(&c, "abc", "https://example.com/x", "Hello", 100).unwrap();
        assert!(is_bookmarked(&c, "abc"));
        assert!(load_ids(&c).contains("abc"));
        remove(&c, "abc").unwrap();
        assert!(!is_bookmarked(&c, "abc"));
        assert!(load_ids(&c).is_empty());
    }

    #[test]
    fn add_is_upsert() {
        let c = fresh_conn();
        add(&c, "abc", "https://a", "Old title", 100).unwrap();
        add(&c, "abc", "https://a", "New title", 200).unwrap();
        let all = list(&c).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "New title");
        assert_eq!(all[0].bookmarked_at, 200);
    }

    #[test]
    fn list_is_newest_first() {
        let c = fresh_conn();
        add(&c, "a", "https://a", "A", 100).unwrap();
        add(&c, "b", "https://b", "B", 300).unwrap();
        add(&c, "c", "https://c", "C", 200).unwrap();
        let ids: Vec<_> = list(&c)
            .unwrap()
            .into_iter()
            .map(|b| b.article_id)
            .collect();
        assert_eq!(ids, vec!["b", "c", "a"]);
    }
}
