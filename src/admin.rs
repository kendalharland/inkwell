//! Admin: DB-backed persistence for feeds and groups.
//!
//! Persistence model:
//!
//! 1. Feeds and groups live in `feed_group` + `feed_subscription` in the
//!    same SQLite file used by the article cache. They survive container
//!    recreations because the DB sits under `/data`, while the YAML
//!    config is baked into the image.
//! 2. On startup, if `feed_group` is empty, [`ensure_seeded`] populates
//!    both tables from `config.rss`. After the first run the config's
//!    `rss:` section is effectively read-only documentation / first-run
//!    seed — admin mutations write to the DB.
//! 3. After every mutation, [`apply_from_db`] rebuilds the index-keyed
//!    runtime state and clears the feed cache so a feed that just
//!    shifted index isn't read against a stale cache entry.

use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::{
    config::ConfigFile,
    state::{AppState, GroupInfo},
};

/// Schema for tables this module owns plus the global PRAGMAs the rest
/// of the process relies on. Run once at startup against the connection
/// that will end up in [`AppState::db`].
pub const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS article (
    id TEXT PRIMARY KEY,
    url TEXT,
    title TEXT,
    html TEXT,
    fetched_at INTEGER
);
CREATE INDEX IF NOT EXISTS article_fetched_at_idx ON article(fetched_at);

CREATE TABLE IF NOT EXISTS feed_group (
    name TEXT PRIMARY KEY,
    position INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS feed_subscription (
    group_name TEXT NOT NULL,
    url TEXT NOT NULL,
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (group_name, url),
    FOREIGN KEY (group_name) REFERENCES feed_group(name) ON DELETE CASCADE
);

-- Bookmarks (\"read later\") store title + url alongside the article id
-- so an entry stays visible even after its feed rolls it off — the
-- listing handler doesn't need to JOIN against `article` to render.
CREATE TABLE IF NOT EXISTS bookmark (
    article_id TEXT PRIMARY KEY,
    url TEXT NOT NULL,
    title TEXT NOT NULL,
    bookmarked_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS bookmark_bookmarked_at_idx ON bookmark(bookmarked_at);
";

/// First-run seed. No-op if the DB already has any groups; otherwise
/// copies `config.rss` into the tables, preserving the configured
/// ordering of both groups and the feeds within each group.
pub fn ensure_seeded(conn: &Connection, cfg: &ConfigFile) -> Result<()> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM feed_group", [], |r| r.get(0))
        .context("counting feed_group")?;
    if n > 0 {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    for (gpos, g) in cfg.rss.groups.iter().enumerate() {
        tx.execute(
            "INSERT OR IGNORE INTO feed_group (name, position) VALUES (?1, ?2)",
            params![&g.name, gpos as i64],
        )?;
        for (fpos, url) in g.feeds.iter().enumerate() {
            tx.execute(
                "INSERT OR IGNORE INTO feed_subscription (group_name, url, position) \
                 VALUES (?1, ?2, ?3)",
                params![&g.name, url, fpos as i64],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Project the DB into the index-keyed runtime layout that handlers and
/// jobs read from. Used both at startup and after every admin write.
///
/// Feeds are deduplicated across groups — a URL listed in groups A and B
/// resolves to one entry in the returned `feeds` vec and the same index
/// in both groups' `feed_indices`.
pub fn build_runtime_state(
    conn: &Connection,
) -> Result<(Vec<String>, Vec<Option<String>>, Vec<GroupInfo>)> {
    let mut feeds: Vec<String> = Vec::new();
    let mut url_to_idx: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<GroupInfo> = Vec::new();

    let group_names: Vec<String> = conn
        .prepare("SELECT name FROM feed_group ORDER BY position, name")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;

    let mut feed_stmt = conn.prepare(
        "SELECT url FROM feed_subscription WHERE group_name = ?1 ORDER BY position, rowid",
    )?;
    for name in group_names {
        let urls: Vec<String> = feed_stmt
            .query_map(params![&name], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        let mut idxs = Vec::with_capacity(urls.len());
        for url in urls {
            let i = *url_to_idx.entry(url.clone()).or_insert_with(|| {
                let i = feeds.len();
                feeds.push(url);
                i
            });
            idxs.push(i);
        }
        groups.push(GroupInfo {
            name,
            feed_indices: idxs,
        });
    }
    let titles = vec![None; feeds.len()];
    Ok((feeds, titles, groups))
}

/// Re-read DB → swap runtime state under write-locks → clear feed cache.
/// Called after every successful mutation so the change is visible
/// without a server restart. The feed cache is cleared because indices
/// may have shifted and stale entries would point at the wrong feed.
pub async fn apply_from_db(state: &AppState) -> Result<()> {
    let (feeds, titles, groups) = {
        let conn = state.db.lock().await;
        build_runtime_state(&conn)?
    };
    *state.feeds.write().await = feeds;
    *state.feed_titles.write().await = titles;
    *state.groups.write().await = groups;
    state.feed_cache.write().await.clear();
    Ok(())
}

// ----- Validation (pure) -------------------------------------------------

pub fn validate_feed_url(url: &str) -> Result<&str> {
    let url = url.trim();
    if url.is_empty() {
        anyhow::bail!("url is empty");
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        anyhow::bail!("url must start with http:// or https://");
    }
    Ok(url)
}

pub fn validate_group_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("group name is empty");
    }
    Ok(name)
}

// ----- DB-backed mutations ----------------------------------------------

pub async fn add_feed_to_group(state: &AppState, group: &str, url: &str) -> Result<()> {
    let url = validate_feed_url(url)?.to_string();
    {
        let conn = state.db.lock().await;
        let exists: bool = conn
            .query_row("SELECT 1 FROM feed_group WHERE name = ?1", [group], |_| {
                Ok(true)
            })
            .unwrap_or(false);
        if !exists {
            anyhow::bail!("group {:?} does not exist", group);
        }
        let pos: i64 = conn.query_row(
            "SELECT COALESCE(MAX(position)+1, 0) FROM feed_subscription WHERE group_name = ?1",
            [group],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO feed_subscription (group_name, url, position) \
             VALUES (?1, ?2, ?3)",
            params![group, &url, pos],
        )?;
    }
    apply_from_db(state).await
}

pub async fn remove_feed_from_group(state: &AppState, group: &str, url: &str) -> Result<()> {
    {
        let conn = state.db.lock().await;
        conn.execute(
            "DELETE FROM feed_subscription WHERE group_name = ?1 AND url = ?2",
            params![group, url],
        )?;
    }
    apply_from_db(state).await
}

pub async fn add_group(state: &AppState, name: &str) -> Result<()> {
    let name = validate_group_name(name)?.to_string();
    {
        let conn = state.db.lock().await;
        let exists: bool = conn
            .query_row("SELECT 1 FROM feed_group WHERE name = ?1", [&name], |_| {
                Ok(true)
            })
            .unwrap_or(false);
        if exists {
            anyhow::bail!("group {:?} already exists", name);
        }
        let pos: i64 = conn.query_row(
            "SELECT COALESCE(MAX(position)+1, 0) FROM feed_group",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO feed_group (name, position) VALUES (?1, ?2)",
            params![&name, pos],
        )?;
    }
    apply_from_db(state).await
}

pub async fn remove_group(state: &AppState, name: &str) -> Result<()> {
    {
        let conn = state.db.lock().await;
        let n = conn.execute("DELETE FROM feed_group WHERE name = ?1", [name])?;
        if n == 0 {
            anyhow::bail!("group {:?} does not exist", name);
        }
        // feed_subscription rows for this group are cascaded by the FK.
    }
    apply_from_db(state).await
}

/// Summary returned by [`import_opml`] for the admin flash message and
/// for tests that need to assert per-category counts.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub groups_created: usize,
    pub feeds_added: usize,
    pub skipped_invalid: usize,
    pub skipped_duplicate: usize,
}

/// Import OPML into the DB and rebuild runtime state. Semantics:
///
/// * Groups already present in the DB are reused; the import only
///   appends feeds. Re-importing the same OPML is therefore a no-op
///   (every URL collides and lands in `skipped_duplicate`).
/// * Feed URLs go through [`validate_feed_url`] so the OPML can't
///   smuggle in `javascript:` / `file://` schemes — this is the same
///   allow-list the single-feed admin form uses.
/// * The whole import runs in one SQLite transaction; if a row fails
///   to insert for any reason other than collision/scheme, the entire
///   import is rolled back and no state change is published to the
///   in-memory `AppState`.
pub async fn import_opml(state: &AppState, xml: &str) -> Result<ImportSummary> {
    let entries = crate::opml::parse(xml)?;
    let mut summary = ImportSummary::default();
    if entries.is_empty() {
        return Ok(summary);
    }

    {
        let conn = state.db.lock().await;
        let tx = conn.unchecked_transaction()?;
        for entry in entries {
            let url = match validate_feed_url(&entry.url) {
                Ok(u) => u.to_string(),
                Err(_) => {
                    summary.skipped_invalid += 1;
                    continue;
                }
            };
            // Group label normalization mirrors `validate_group_name`;
            // we can't call it directly because the parser already
            // trimmed and we want to count empties as `skipped_invalid`
            // rather than aborting the whole transaction.
            let group = entry.group.trim();
            if group.is_empty() {
                summary.skipped_invalid += 1;
                continue;
            }

            let exists: bool = tx
                .query_row("SELECT 1 FROM feed_group WHERE name = ?1", [group], |_| {
                    Ok(true)
                })
                .unwrap_or(false);
            if !exists {
                let pos: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(position)+1, 0) FROM feed_group",
                    [],
                    |r| r.get(0),
                )?;
                tx.execute(
                    "INSERT INTO feed_group (name, position) VALUES (?1, ?2)",
                    params![group, pos],
                )?;
                summary.groups_created += 1;
            }

            let pos: i64 = tx.query_row(
                "SELECT COALESCE(MAX(position)+1, 0) FROM feed_subscription \
                 WHERE group_name = ?1",
                [group],
                |r| r.get(0),
            )?;
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO feed_subscription (group_name, url, position) \
                 VALUES (?1, ?2, ?3)",
                params![group, &url, pos],
            )?;
            if inserted == 0 {
                summary.skipped_duplicate += 1;
            } else {
                summary.feeds_added += 1;
            }
        }
        tx.commit()?;
    }
    apply_from_db(state).await?;
    Ok(summary)
}

/// Read the full current config back out of the DB. Used by the admin
/// UI to render the list of groups and their feeds; doesn't touch the
/// in-memory `AppState` because the admin renderer can serve straight
/// from the DB without going through the dedup layer.
pub struct GroupView {
    pub name: String,
    pub feeds: Vec<String>,
}

pub async fn list_groups(state: &AppState) -> Result<Vec<GroupView>> {
    let conn = state.db.lock().await;
    let names: Vec<String> = conn
        .prepare("SELECT name FROM feed_group ORDER BY position, name")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    let mut feed_stmt = conn.prepare(
        "SELECT url FROM feed_subscription WHERE group_name = ?1 ORDER BY position, rowid",
    )?;
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let feeds: Vec<String> = feed_stmt
            .query_map(params![&name], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        out.push(GroupView { name, feeds });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GroupConfig, RssConfig, ViewConfig};

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn empty_config() -> ConfigFile {
        ConfigFile {
            rss: RssConfig { groups: Vec::new() },
            scheduler: None,
            view: ViewConfig::default(),
            gemini: None,
            feed_search: crate::config::FeedSearchConfig::default(),
        }
    }

    fn config_with(groups: &[(&str, &[&str])]) -> ConfigFile {
        let mut c = empty_config();
        for (name, feeds) in groups {
            c.rss.groups.push(GroupConfig {
                name: (*name).into(),
                feeds: feeds.iter().map(|s| s.to_string()).collect(),
            });
        }
        c
    }

    fn group_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM feed_group", [], |r| r.get(0))
            .unwrap()
    }

    fn feed_count(conn: &Connection, group: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM feed_subscription WHERE group_name = ?1",
            [group],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn validate_feed_url_rejects_non_http() {
        assert!(validate_feed_url("ftp://x").is_err());
        assert!(validate_feed_url("javascript:alert(1)").is_err());
        assert!(validate_feed_url("").is_err());
        assert!(validate_feed_url("   ").is_err());
        assert_eq!(
            validate_feed_url("  https://x.com/rss  ").unwrap(),
            "https://x.com/rss"
        );
    }

    #[test]
    fn validate_feed_url_blocks_all_dangerous_schemes() {
        // Regression for #18: `POST /bookmark/{iid}` was missing a
        // scheme check, so `javascript:` / `data:` slipped into the
        // `bookmark.url` column and later rendered in <a href> on the
        // article page. The bookmark handler now reuses
        // `validate_feed_url`, so this set has to stay rejected.
        for bad in [
            "javascript:alert(document.cookie)",
            "JAVASCRIPT:alert(1)", // case still doesn't start with http(s)://
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            "file:///etc/passwd",
            "//evil.com/x", // scheme-relative — no http(s):// prefix
        ] {
            assert!(
                validate_feed_url(bad).is_err(),
                "{:?} should be rejected",
                bad
            );
        }
    }

    #[test]
    fn validate_group_name_rejects_empty() {
        assert!(validate_group_name("   ").is_err());
        assert_eq!(validate_group_name("  tech  ").unwrap(), "tech");
    }

    #[test]
    fn ensure_seeded_populates_empty_db() {
        let conn = fresh_conn();
        let cfg = config_with(&[("a", &["https://a"]), ("b", &["https://b", "https://c"])]);
        ensure_seeded(&conn, &cfg).unwrap();
        assert_eq!(group_count(&conn), 2);
        assert_eq!(feed_count(&conn, "a"), 1);
        assert_eq!(feed_count(&conn, "b"), 2);
    }

    #[test]
    fn ensure_seeded_is_idempotent_and_does_not_overwrite_db_changes() {
        let conn = fresh_conn();
        // First-run seed.
        ensure_seeded(&conn, &config_with(&[("a", &["https://a"])])).unwrap();
        // Simulate an admin add via the DB.
        conn.execute(
            "INSERT INTO feed_subscription (group_name, url, position) VALUES (?1, ?2, ?3)",
            params!["a", "https://added-by-admin", 1_i64],
        )
        .unwrap();
        // Re-seed with a different config: must be a no-op because the
        // DB is non-empty. Admin's added feed survives; the new config's
        // feed is ignored.
        ensure_seeded(&conn, &config_with(&[("a", &["https://stale-config"])])).unwrap();
        let urls: Vec<String> = conn
            .prepare("SELECT url FROM feed_subscription WHERE group_name = 'a' ORDER BY position")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(urls, vec!["https://a", "https://added-by-admin"]);
    }

    #[test]
    fn build_runtime_state_dedupes_feeds_across_groups() {
        let conn = fresh_conn();
        let cfg = config_with(&[
            ("a", &["https://shared", "https://only-a"]),
            ("b", &["https://shared", "https://only-b"]),
        ]);
        ensure_seeded(&conn, &cfg).unwrap();
        let (feeds, titles, groups) = build_runtime_state(&conn).unwrap();
        assert_eq!(feeds.len(), 3, "shared feed counts once");
        assert_eq!(titles.len(), 3);
        assert_eq!(groups[0].feed_indices.len(), 2);
        assert_eq!(groups[1].feed_indices.len(), 2);
        assert_eq!(groups[0].feed_indices[0], groups[1].feed_indices[0]);
    }

    #[test]
    fn build_runtime_state_preserves_configured_order() {
        let conn = fresh_conn();
        // Use a config whose alphabetical and configured orders differ.
        let cfg = config_with(&[("zeta", &["https://z"]), ("alpha", &["https://a"])]);
        ensure_seeded(&conn, &cfg).unwrap();
        let (_, _, groups) = build_runtime_state(&conn).unwrap();
        assert_eq!(groups[0].name, "zeta");
        assert_eq!(groups[1].name, "alpha");
    }

    #[test]
    fn remove_group_cascades_to_feed_subscription() {
        let conn = fresh_conn();
        ensure_seeded(
            &conn,
            &config_with(&[("a", &["https://a1", "https://a2"]), ("b", &["https://b1"])]),
        )
        .unwrap();
        conn.execute("DELETE FROM feed_group WHERE name = 'a'", [])
            .unwrap();
        assert_eq!(feed_count(&conn, "a"), 0);
        assert_eq!(feed_count(&conn, "b"), 1);
    }

    // ----- async public-API tests -----------------------------------------
    //
    // Everything above exercises the schema-level fns directly against an
    // in-memory `Connection`. These tests cover the async `add_*` /
    // `remove_*` entry points the route handlers actually call, including
    // their `apply_from_db` propagation into the live `AppState`.

    use std::sync::Arc;
    use tokio::sync::{Mutex, RwLock};

    fn fresh_app_state() -> Arc<AppState> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Arc::new(AppState {
            feeds: RwLock::new(Vec::new()),
            feed_titles: RwLock::new(Vec::new()),
            groups: RwLock::new(Vec::new()),
            http: reqwest::Client::new(),
            discovery_http: reqwest::Client::new(),
            feed_cache: RwLock::new(std::collections::HashMap::new()),
            db: Mutex::new(conn),
            feed_ttl: std::time::Duration::from_secs(60),
            article_ttl_secs: 86400,
            compact_default: false,
            dark_default: false,
            feed_search: crate::config::FeedSearchConfig::default(),
        })
    }

    #[tokio::test]
    async fn add_group_then_add_feed_propagates_to_in_memory_state() {
        let state = fresh_app_state();
        add_group(&state, "tech").await.unwrap();
        add_feed_to_group(&state, "tech", "https://lobste.rs/rss")
            .await
            .unwrap();
        assert_eq!(
            state.feeds.read().await.clone(),
            vec!["https://lobste.rs/rss"]
        );
        let groups = state.groups.read().await;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "tech");
        assert_eq!(groups[0].feed_indices, vec![0_usize]);
    }

    #[tokio::test]
    async fn add_feed_to_missing_group_errors_and_leaves_state_clean() {
        let state = fresh_app_state();
        let err = add_feed_to_group(&state, "missing", "https://x.example/rss")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"));
        assert!(state.feeds.read().await.is_empty());
        let conn = state.db.lock().await;
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM feed_subscription", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn add_feed_rejects_non_http_schemes() {
        // Same scheme allow-list the bookmark handler reuses.
        let state = fresh_app_state();
        add_group(&state, "tech").await.unwrap();
        for bad in [
            "javascript:alert(1)",
            "data:text/html,<script>",
            "file:///etc/passwd",
            "ftp://example.com/feed",
        ] {
            let r = add_feed_to_group(&state, "tech", bad).await;
            assert!(r.is_err(), "{:?} should be rejected", bad);
        }
        assert!(state.feeds.read().await.is_empty());
    }

    #[tokio::test]
    async fn add_group_is_idempotent_per_name() {
        let state = fresh_app_state();
        add_group(&state, "tech").await.unwrap();
        let err = add_group(&state, "tech").await.unwrap_err().to_string();
        assert!(err.contains("already exists"));
        assert_eq!(state.groups.read().await.len(), 1);
    }

    #[tokio::test]
    async fn add_group_rejects_empty_or_whitespace_names() {
        let state = fresh_app_state();
        assert!(add_group(&state, "").await.is_err());
        assert!(add_group(&state, "   ").await.is_err());
        assert!(state.groups.read().await.is_empty());
    }

    #[tokio::test]
    async fn remove_feed_from_group_drops_only_that_subscription() {
        let state = fresh_app_state();
        add_group(&state, "tech").await.unwrap();
        add_feed_to_group(&state, "tech", "https://a/rss")
            .await
            .unwrap();
        add_feed_to_group(&state, "tech", "https://b/rss")
            .await
            .unwrap();
        remove_feed_from_group(&state, "tech", "https://a/rss")
            .await
            .unwrap();
        let groups = state.groups.read().await;
        let feeds = state.feeds.read().await;
        let urls: Vec<String> = groups[0]
            .feed_indices
            .iter()
            .map(|&i| feeds[i].clone())
            .collect();
        assert_eq!(urls, vec!["https://b/rss"]);
    }

    #[tokio::test]
    async fn remove_feed_is_silent_when_not_subscribed() {
        let state = fresh_app_state();
        add_group(&state, "tech").await.unwrap();
        // No error even though the URL was never added.
        remove_feed_from_group(&state, "tech", "https://nope/rss")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn remove_group_cascades_through_public_api() {
        // Whole-flow regression: the public async path goes through
        // apply_from_db, which must reflect the cascade in the live
        // in-memory state, not just the DB.
        let state = fresh_app_state();
        add_group(&state, "tech").await.unwrap();
        add_feed_to_group(&state, "tech", "https://a/rss")
            .await
            .unwrap();
        add_feed_to_group(&state, "tech", "https://b/rss")
            .await
            .unwrap();
        remove_group(&state, "tech").await.unwrap();
        assert!(state.groups.read().await.is_empty());
        assert!(state.feeds.read().await.is_empty());
        let conn = state.db.lock().await;
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM feed_subscription", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "FK cascade should have dropped the rows");
    }

    #[tokio::test]
    async fn remove_group_errors_when_not_found() {
        let state = fresh_app_state();
        let err = remove_group(&state, "missing")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"));
    }

    // ----- OPML import tests ----------------------------------------------

    fn opml(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <opml version="2.0"><head><title>x</title></head><body>{body}</body></opml>"#
        )
    }

    async fn group_urls(state: &AppState, group: &str) -> Vec<String> {
        let conn = state.db.lock().await;
        let mut stmt = conn
            .prepare("SELECT url FROM feed_subscription WHERE group_name = ?1 ORDER BY position")
            .unwrap();
        stmt.query_map([group], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[tokio::test]
    async fn import_opml_into_empty_db_creates_groups_and_feeds() {
        let state = fresh_app_state();
        let xml = opml(
            r#"<outline text="Tech">
                <outline xmlUrl="https://a.example/rss"/>
                <outline xmlUrl="https://b.example/rss"/>
              </outline>"#,
        );
        let s = import_opml(&state, &xml).await.unwrap();
        assert_eq!(
            s,
            ImportSummary {
                groups_created: 1,
                feeds_added: 2,
                skipped_invalid: 0,
                skipped_duplicate: 0,
            }
        );
        assert_eq!(
            group_urls(&state, "Tech").await,
            vec!["https://a.example/rss", "https://b.example/rss"]
        );
    }

    #[tokio::test]
    async fn import_opml_appends_to_existing_group_without_recounting_it() {
        // Existing group must not increment `groups_created`; feeds that
        // are already there must not increment `feeds_added`.
        let state = fresh_app_state();
        add_group(&state, "Tech").await.unwrap();
        add_feed_to_group(&state, "Tech", "https://a.example/rss")
            .await
            .unwrap();

        let xml = opml(
            r#"<outline text="Tech">
                <outline xmlUrl="https://a.example/rss"/>
                <outline xmlUrl="https://b.example/rss"/>
              </outline>"#,
        );
        let s = import_opml(&state, &xml).await.unwrap();
        assert_eq!(s.groups_created, 0);
        assert_eq!(s.feeds_added, 1);
        assert_eq!(s.skipped_duplicate, 1);
        assert_eq!(
            group_urls(&state, "Tech").await,
            vec!["https://a.example/rss", "https://b.example/rss"]
        );
    }

    #[tokio::test]
    async fn import_opml_is_idempotent_when_run_twice() {
        // Re-importing the same file changes nothing — every URL
        // collides on the second pass and lands in `skipped_duplicate`.
        let state = fresh_app_state();
        let xml = opml(
            r#"<outline text="Tech">
                 <outline xmlUrl="https://a.example/rss"/>
               </outline>"#,
        );
        let first = import_opml(&state, &xml).await.unwrap();
        let second = import_opml(&state, &xml).await.unwrap();
        assert_eq!(first.feeds_added, 1);
        assert_eq!(second.feeds_added, 0);
        assert_eq!(second.skipped_duplicate, 1);
        assert_eq!(second.groups_created, 0);
        assert_eq!(group_urls(&state, "Tech").await.len(), 1);
    }

    #[tokio::test]
    async fn import_opml_skips_javascript_and_file_schemes() {
        // Reuses validate_feed_url, so the same scheme allow-list that
        // protects the single-feed admin form protects OPML import.
        let state = fresh_app_state();
        let xml = opml(
            r#"<outline text="Bad">
                <outline xmlUrl="javascript:alert(1)"/>
                <outline xmlUrl="file:///etc/passwd"/>
                <outline xmlUrl="ftp://example.com/feed"/>
                <outline xmlUrl="https://ok.example/rss"/>
              </outline>"#,
        );
        let s = import_opml(&state, &xml).await.unwrap();
        assert_eq!(s.feeds_added, 1);
        assert_eq!(s.skipped_invalid, 3);
        assert_eq!(s.skipped_duplicate, 0);
        assert_eq!(
            group_urls(&state, "Bad").await,
            vec!["https://ok.example/rss"]
        );
    }

    #[tokio::test]
    async fn import_opml_top_level_feeds_land_in_uncategorized() {
        let state = fresh_app_state();
        let xml = opml(r#"<outline xmlUrl="https://loose.example/rss"/>"#);
        let s = import_opml(&state, &xml).await.unwrap();
        assert_eq!(s.groups_created, 1);
        assert_eq!(s.feeds_added, 1);
        let groups = list_groups(&state).await.unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, crate::opml::UNCATEGORIZED_GROUP);
    }

    #[tokio::test]
    async fn import_opml_propagates_to_runtime_app_state() {
        // Apply_from_db must run after the import — otherwise the
        // imported feeds are persisted but invisible until restart.
        let state = fresh_app_state();
        let xml = opml(
            r#"<outline text="Tech">
                <outline xmlUrl="https://a.example/rss"/>
              </outline>"#,
        );
        import_opml(&state, &xml).await.unwrap();
        assert_eq!(
            state.feeds.read().await.clone(),
            vec!["https://a.example/rss"]
        );
        let groups = state.groups.read().await;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Tech");
        assert_eq!(groups[0].feed_indices, vec![0_usize]);
    }

    #[tokio::test]
    async fn import_opml_empty_body_yields_zero_summary() {
        let state = fresh_app_state();
        let s = import_opml(&state, &opml("")).await.unwrap();
        assert_eq!(s, ImportSummary::default());
        assert!(list_groups(&state).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn import_opml_malformed_xml_errors_and_leaves_db_clean() {
        let state = fresh_app_state();
        let bad = r#"<?xml version="1.0"?><opml><body><outline xmlUrl="x"></body></opml>"#;
        assert!(import_opml(&state, bad).await.is_err());
        assert!(list_groups(&state).await.unwrap().is_empty());
        assert!(state.feeds.read().await.is_empty());
    }

    #[tokio::test]
    async fn import_opml_dedups_duplicates_within_same_file() {
        let state = fresh_app_state();
        let xml = opml(
            r#"<outline text="Tech">
                <outline xmlUrl="https://x.example/rss"/>
                <outline xmlUrl="https://x.example/rss"/>
                <outline xmlUrl="https://x.example/rss"/>
              </outline>"#,
        );
        let s = import_opml(&state, &xml).await.unwrap();
        assert_eq!(s.feeds_added, 1);
        assert_eq!(s.skipped_duplicate, 2);
        assert_eq!(group_urls(&state, "Tech").await.len(), 1);
    }

    #[tokio::test]
    async fn import_opml_appends_in_document_order() {
        // The append order matters for the admin UI's list rendering;
        // pin it so a future change to position bookkeeping shows up.
        let state = fresh_app_state();
        add_group(&state, "Tech").await.unwrap();
        add_feed_to_group(&state, "Tech", "https://existing.example/rss")
            .await
            .unwrap();
        let xml = opml(
            r#"<outline text="Tech">
                <outline xmlUrl="https://new1.example/rss"/>
                <outline xmlUrl="https://new2.example/rss"/>
              </outline>"#,
        );
        import_opml(&state, &xml).await.unwrap();
        assert_eq!(
            group_urls(&state, "Tech").await,
            vec![
                "https://existing.example/rss",
                "https://new1.example/rss",
                "https://new2.example/rss",
            ]
        );
    }

    #[tokio::test]
    async fn import_opml_creates_only_new_groups() {
        // Pre-create one of the two groups. Only the second should
        // count toward `groups_created`.
        let state = fresh_app_state();
        add_group(&state, "Existing").await.unwrap();
        let xml = opml(
            r#"<outline text="Existing">
                <outline xmlUrl="https://a.example/rss"/>
              </outline>
              <outline text="Brand New">
                <outline xmlUrl="https://b.example/rss"/>
              </outline>"#,
        );
        let s = import_opml(&state, &xml).await.unwrap();
        assert_eq!(s.groups_created, 1, "only Brand New is new");
        assert_eq!(s.feeds_added, 2);
    }

    #[tokio::test]
    async fn import_opml_groups_inherited_from_nested_categories() {
        // Re-verifies the parser's nearest-ancestor rule end-to-end
        // against the DB: a feed under <Tech><Programming><feed/>>
        // should be stored under "Programming", not "Tech".
        let state = fresh_app_state();
        let xml = opml(
            r#"<outline text="Tech">
                <outline text="Programming">
                  <outline xmlUrl="https://prog.example/rss"/>
                </outline>
               </outline>"#,
        );
        import_opml(&state, &xml).await.unwrap();
        assert_eq!(
            group_urls(&state, "Programming").await,
            vec!["https://prog.example/rss"]
        );
        // "Tech" exists as a label but has no feeds of its own.
        assert!(group_urls(&state, "Tech").await.is_empty());
    }

    #[tokio::test]
    async fn list_groups_reflects_admin_mutations_in_order() {
        let state = fresh_app_state();
        add_group(&state, "first").await.unwrap();
        add_group(&state, "second").await.unwrap();
        add_feed_to_group(&state, "first", "https://x/rss")
            .await
            .unwrap();
        let groups = list_groups(&state).await.unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "first");
        assert_eq!(groups[0].feeds, vec!["https://x/rss"]);
        assert_eq!(groups[1].name, "second");
        assert!(groups[1].feeds.is_empty());
    }
}
