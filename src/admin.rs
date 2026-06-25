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
}
