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
                let link = e
                    .links
                    .first()
                    .map(|l| l.href.clone())
                    .unwrap_or_default();
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
