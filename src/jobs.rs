//! Honker-backed background jobs (feed refresh + article purge) and the
//! worker loop that drives them.
//!
//! Safety choices (also called out in the crate-level doc):
//!
//! * **Idempotent registration.** `reconcile_schedules` removes-then-adds
//!   every startup so a YAML edit to the cron string takes effect on the
//!   next restart. `Scheduler::add` is idempotent by name (per honker
//!   docstring), so even without the explicit remove the worst case is a
//!   no-op.
//! * **No double scheduling across instances.** `Scheduler::run` performs
//!   SQLite-level leader election with a 60s lock TTL — running two inkwell
//!   processes against the same DB is safe; only one fires schedules. The
//!   leader entry is visible in `_honker_locks`.
//! * **No backlog pile-up.** `worker_loop` claims jobs in batches and
//!   runs the handler at most once per batch. If a previous handler is
//!   still in flight (in-process `Mutex::try_lock`), the batch is ack'd
//!   without running.
//! * **No poison pills.** Failed handlers are logged and ack'd, not
//!   `fail()`'d into the retry queue. For periodic refresh this is the
//!   right semantic: the next tick will try again.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::{
    extract::{extract_url, sanitize_images, BLOCKED_MARKER},
    feeds::{ensure_feeds, entry_full_html, item_id},
    state::AppState,
    view::now_secs,
};

pub const REFRESH_SCHEDULE_NAME: &str = "inkwell.refresh";
pub const REFRESH_QUEUE_NAME: &str = "inkwell.refresh";
pub const PURGE_SCHEDULE_NAME: &str = "inkwell.purge";
pub const PURGE_QUEUE_NAME: &str = "inkwell.purge";
pub const SCHEDULER_OWNER: &str = "inkwell-scheduler";

/// Force-refresh every feed (ignoring per-feed TTL), then for every entry
/// not already in the SQLite article cache, run the full
/// fetch + readability pipeline and store the result.
///
/// Returns `(entries_seen, new_articles_extracted)` so the worker can log
/// a useful summary.
pub async fn run_refresh(state: Arc<AppState>) -> Result<(usize, usize)> {
    let n_feeds = state.feeds.read().await.len();
    let all_idxs: Vec<usize> = (0..n_feeds).collect();
    ensure_feeds(state.clone(), &all_idxs, true).await;

    // Snapshot the work-list up front. Holding the cache read-lock across
    // hundreds of HTTP fetches would block every reader handler.
    let work: Vec<(String, String, String, Option<String>)> = {
        let cache = state.feed_cache.read().await;
        let mut out = Vec::new();
        for &i in &all_idxs {
            let Some(cf) = cache.get(&i) else { continue };
            for e in &cf.parsed.entries {
                let link = e.links.first().map(|l| l.href.clone()).unwrap_or_default();
                if link.is_empty() {
                    continue;
                }
                let iid = item_id(&link);
                let title = e
                    .title
                    .as_ref()
                    .map(|t| t.content.clone())
                    .unwrap_or_else(|| link.clone());
                out.push((iid, link, title, entry_full_html(e)));
            }
        }
        out
    };

    let total = work.len();
    let mut new_extractions = 0usize;
    for (iid, link, title, full) in work {
        // Skip anything already cached.
        let exists = {
            let conn = state.db.lock().await;
            conn.query_row("SELECT 1 FROM article WHERE id=?1", [&iid], |_| {
                Ok::<_, rusqlite::Error>(())
            })
            .is_ok()
        };
        if exists {
            continue;
        }
        let html = if let Some(h) = full {
            sanitize_images(&h)
        } else {
            extract_url(&state.http, &link).await
        };
        if html.contains(BLOCKED_MARKER) {
            // Don't persist blocked responses — a re-tap should retry.
            continue;
        }
        let conn = state.db.lock().await;
        let _ = conn.execute(
            "INSERT OR REPLACE INTO article (id,url,title,html,fetched_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![&iid, &link, &title, &html, now_secs()],
        );
        new_extractions += 1;
    }
    Ok((total, new_extractions))
}

/// Delete cached articles older than `article_ttl_secs`, except for
/// articles the user bookmarked into the "read later" pane — those are
/// pinned past the TTL so a saved entry doesn't silently vanish.
/// Returns the number of rows removed.
pub async fn run_purge(state: Arc<AppState>) -> Result<usize> {
    let cutoff = now_secs() - state.article_ttl_secs;
    let n = {
        let conn = state.db.lock().await;
        conn.execute(
            "DELETE FROM article \
             WHERE fetched_at < ?1 \
               AND id NOT IN (SELECT article_id FROM bookmark)",
            [cutoff],
        )?
    };
    Ok(n)
}

/// Generic worker loop. Claims jobs in batches (so backlog is coalesced),
/// runs the handler at most once per batch, and ack's every claimed job.
///
/// `lock` is the in-process mutex that prevents an overlapping run if the
/// previous one hasn't finished. If `try_lock` fails the batch is ack'd
/// without invoking the handler — a long refresh therefore cannot stack
/// backlog.
pub async fn worker_loop<F, Fut>(
    queue: honker::Queue,
    state: Arc<AppState>,
    lock: Arc<Mutex<()>>,
    label: &'static str,
    handler: F,
    stop: Arc<AtomicBool>,
) where
    F: Fn(Arc<AppState>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<String>> + Send,
{
    let worker_id = format!("inkwell.{}.0", label);
    while !stop.load(Ordering::Relaxed) {
        let q = queue.clone();
        let wid = worker_id.clone();
        let claimed = tokio::task::spawn_blocking(move || q.claim_batch(&wid, 32))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();

        if claimed.is_empty() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        let ids: Vec<i64> = claimed.iter().map(|j| j.id).collect();
        tracing::debug!("{}: claimed {} job(s)", label, ids.len());

        match lock.try_lock() {
            Ok(_guard) => match handler(state.clone()).await {
                Ok(summary) => tracing::info!("{}: ok ({})", label, summary),
                Err(e) => tracing::error!("{}: handler failed: {}", label, e),
            },
            Err(_) => tracing::warn!(
                "{}: previous run still in flight, skipping {} job(s)",
                label,
                ids.len()
            ),
        }

        let q = queue.clone();
        let wid = worker_id.clone();
        match tokio::task::spawn_blocking(move || q.ack_batch(&ids, &wid)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::error!("{}: ack_batch failed: {}", label, e),
            Err(e) => tracing::error!("{}: ack worker panicked: {}", label, e),
        }
    }
}

/// Idempotently reconcile the desired schedules with what's already in the
/// honker tables. We `remove` first so that a YAML edit to the cron string
/// takes effect on the next restart even though `add` is idempotent
/// by-name (idempotent-add wouldn't overwrite a changed cron expression).
pub fn reconcile_schedules(
    scheduler: &honker::Scheduler,
    refresh_cron: &str,
    purge_cron: &str,
) -> Result<()> {
    let _ = scheduler.remove(REFRESH_SCHEDULE_NAME);
    let _ = scheduler.remove(PURGE_SCHEDULE_NAME);
    scheduler
        .add(honker::ScheduledTask {
            name: REFRESH_SCHEDULE_NAME.to_string(),
            queue: REFRESH_QUEUE_NAME.to_string(),
            schedule: refresh_cron.to_string(),
            payload: serde_json::json!({}),
            priority: 0,
            expires_s: None,
        })
        .with_context(|| format!("registering refresh schedule '{}'", refresh_cron))?;
    scheduler
        .add(honker::ScheduledTask {
            name: PURGE_SCHEDULE_NAME.to_string(),
            queue: PURGE_QUEUE_NAME.to_string(),
            schedule: purge_cron.to_string(),
            payload: serde_json::json!({}),
            priority: 0,
            expires_s: None,
        })
        .with_context(|| format!("registering purge schedule '{}'", purge_cron))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    fn fresh_state(article_ttl_days: i64) -> Arc<AppState> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::admin::SCHEMA).unwrap();
        Arc::new(AppState {
            feeds: RwLock::new(Vec::new()),
            feed_titles: RwLock::new(Vec::new()),
            groups: RwLock::new(Vec::new()),
            http: reqwest::Client::new(),
            discovery_http: reqwest::Client::new(),
            feed_cache: RwLock::new(HashMap::new()),
            db: Mutex::new(conn),
            feed_ttl: Duration::from_secs(60),
            article_ttl_secs: article_ttl_days * 86400,
            compact_default: false,
            dark_default: false,
            feed_search: crate::config::FeedSearchConfig::default(),
        })
    }

    async fn seed_article(state: &AppState, id: &str, fetched_at: i64) {
        let conn = state.db.lock().await;
        conn.execute(
            "INSERT INTO article (id, url, title, html, fetched_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, "https://example.com/x", "T", "<p>body</p>", fetched_at],
        )
        .unwrap();
    }

    async fn bookmark(state: &AppState, id: &str) {
        let conn = state.db.lock().await;
        crate::bookmarks::add(&conn, id, "https://example.com/x", "T", 0).unwrap();
    }

    async fn article_ids(state: &AppState) -> Vec<String> {
        let conn = state.db.lock().await;
        let mut stmt = conn.prepare("SELECT id FROM article ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[tokio::test]
    async fn run_purge_deletes_only_articles_older_than_ttl() {
        let state = fresh_state(30);
        let fresh = now_secs() - 1; // well within TTL
        let stale = now_secs() - 365 * 86400; // a year old
        seed_article(&state, "fresh", fresh).await;
        seed_article(&state, "stale", stale).await;
        let removed = run_purge(state.clone()).await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(article_ids(&state).await, vec!["fresh"]);
    }

    #[tokio::test]
    async fn run_purge_pins_bookmarked_articles_past_ttl() {
        // The user-visible promise of the read-later flow: a saved
        // article doesn't silently vanish even if the feed has rolled
        // it off and the purge job has run since.
        let state = fresh_state(30);
        let stale = now_secs() - 365 * 86400;
        seed_article(&state, "saved", stale).await;
        seed_article(&state, "unsaved", stale).await;
        bookmark(&state, "saved").await;
        let removed = run_purge(state.clone()).await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(article_ids(&state).await, vec!["saved"]);
    }

    #[tokio::test]
    async fn run_purge_no_op_when_nothing_old_enough() {
        let state = fresh_state(30);
        let fresh = now_secs() - 1;
        seed_article(&state, "a", fresh).await;
        seed_article(&state, "b", fresh).await;
        let removed = run_purge(state.clone()).await.unwrap();
        assert_eq!(removed, 0);
        assert_eq!(article_ids(&state).await, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn run_purge_drops_unbookmarked_article_after_unbookmark() {
        // Bookmarks pin past the TTL only while the bookmark exists;
        // removing the bookmark restores the row's eligibility for
        // deletion on the next purge tick. This is the inverse of the
        // pinning guarantee — both are load-bearing.
        let state = fresh_state(30);
        let stale = now_secs() - 365 * 86400;
        seed_article(&state, "saved-then-not", stale).await;
        bookmark(&state, "saved-then-not").await;
        // First purge: bookmark protects.
        assert_eq!(run_purge(state.clone()).await.unwrap(), 0);
        // Remove the bookmark.
        {
            let conn = state.db.lock().await;
            crate::bookmarks::remove(&conn, "saved-then-not").unwrap();
        }
        // Second purge: now the article is eligible and goes.
        assert_eq!(run_purge(state.clone()).await.unwrap(), 1);
        assert!(article_ids(&state).await.is_empty());
    }
}
