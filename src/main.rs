//! # inkwell
//!
//! Kindle-friendly reader for RSS/Atom feeds, with an optional honker-backed
//! cron scheduler that pre-extracts articles so taps render instantly.
//!
//! See module-level docs for the design of each layer:
//!
//! * [`config`] — YAML schema
//! * [`state`]  — shared process state
//! * [`feeds`]  — fetch, parse, in-memory cache, entry projection
//! * [`extract`] — live HTTP + readability with blocked-site fallback
//! * [`view`]   — Kindle-targeted HTML rendering + pagination
//! * [`routes`] — axum handlers
//! * [`jobs`]   — honker scheduler integration; refresh + purge handlers
//! * [`logging`] — stderr + optional file logging
//!
//! The crate-level safety properties of the scheduler are documented at the
//! top of [`jobs`].

mod admin;
mod bookmarks;
mod config;
mod extract;
mod feed_search;
mod feeds;
mod gemini;
mod jobs;
mod logging;
mod opml;
mod routes;
mod state;
mod template;
mod view;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};

use crate::{
    config::ConfigFile,
    jobs::{
        reconcile_schedules, run_purge, run_refresh, worker_loop, PURGE_QUEUE_NAME,
        REFRESH_QUEUE_NAME, SCHEDULER_OWNER,
    },
    logging::init_logging,
    state::AppState,
};

#[tokio::main]
async fn main() -> Result<()> {
    let config_path: PathBuf = std::env::args()
        .nth(1)
        .context("usage: inkwell <config.yaml>")?
        .into();
    let config_str = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config: ConfigFile = serde_yml::from_str(&config_str)
        .with_context(|| format!("parsing {}", config_path.display()))?;

    let _log_guard = init_logging(config.scheduler.as_ref().map(|s| s.log_file.as_path()));

    let timeout_secs: u64 = std::env::var("HTTP_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; inkwell-rss-reader/0.1)")
        .timeout(Duration::from_secs(timeout_secs))
        .build()?;
    // Used only by the feed-discovery autocomplete (#15). Redirects
    // are followed manually in `feed_search` so each hop's host can
    // be re-checked against the SSRF block-list.
    let discovery_http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; inkwell-rss-reader/0.1)")
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let db_path = std::env::var("CACHE_DB").unwrap_or_else(|_| "reader_cache.sqlite".into());
    let conn = rusqlite::Connection::open(&db_path)?;
    // WAL is required for two connections (ours + honker's) to share the
    // same file without long writer stalls. NORMAL sync is sufficient
    // durability for a personal cache — losing a few articles on power
    // loss is benign; they'll be re-extracted on next refresh.
    // foreign_keys is needed so removing a group cascades into
    // feed_subscription rows.
    conn.execute_batch(admin::SCHEMA)?;
    admin::ensure_seeded(&conn, &config)?;
    let (feeds, titles_vec, groups) = admin::build_runtime_state(&conn)?;
    if feeds.is_empty() {
        tracing::info!("no feeds yet — add some via the /admin UI");
    }
    let feed_titles = RwLock::new(titles_vec);

    let feed_ttl = Duration::from_secs(
        std::env::var("FEED_TTL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600),
    );

    let article_ttl_secs = config
        .scheduler
        .as_ref()
        .map(|s| s.article_ttl_days as i64 * 86400)
        .unwrap_or(30 * 86400);

    let state = Arc::new(AppState {
        feeds: RwLock::new(feeds),
        feed_titles,
        groups: RwLock::new(groups),
        http,
        discovery_http,
        feed_cache: RwLock::new(HashMap::new()),
        db: Mutex::new(conn),
        feed_ttl,
        article_ttl_secs,
        compact_default: config.view.compact_default,
        dark_default: config.view.dark_default,
        feed_search: config.feed_search.clone(),
    });

    // ---------- scheduler / workers ----------------------------------------
    let stop = Arc::new(AtomicBool::new(false));
    if let Some(sched_cfg) = config.scheduler.clone() {
        let db = honker::Database::open(&db_path)
            .with_context(|| format!("opening honker db at {}", db_path))?;
        {
            let scheduler = db.scheduler();
            reconcile_schedules(&scheduler, &sched_cfg.refresh, &sched_cfg.purge)?;
        }
        tracing::info!(
            "scheduler armed — refresh: '{}', purge: '{}', article_ttl_days: {}",
            sched_cfg.refresh,
            sched_cfg.purge,
            sched_cfg.article_ttl_days
        );

        // Leader-elected scheduler ticker. Blocking loop; runs forever in
        // its own OS thread via spawn_blocking. Other inkwell instances
        // against the same DB will see this lock and stay idle.
        {
            let db = db.clone();
            let stop = stop.clone();
            tokio::task::spawn_blocking(move || {
                let scheduler = db.scheduler();
                if let Err(e) = scheduler.run(stop, SCHEDULER_OWNER) {
                    tracing::error!("scheduler.run exited with error: {}", e);
                }
            });
        }

        // Workers: one per queue. Each has its own in-process try-lock so
        // its handler cannot overlap with itself.
        let refresh_q = db.queue(REFRESH_QUEUE_NAME, honker::QueueOpts::default());
        let purge_q = db.queue(PURGE_QUEUE_NAME, honker::QueueOpts::default());

        let refresh_lock = Arc::new(Mutex::new(()));
        let purge_lock = Arc::new(Mutex::new(()));

        {
            let state = state.clone();
            let stop = stop.clone();
            let lock = refresh_lock.clone();
            tokio::spawn(async move {
                worker_loop(
                    refresh_q.clone(),
                    state,
                    lock,
                    "refresh",
                    |s| async move {
                        let (total, new) = run_refresh(s).await?;
                        Ok(format!(
                            "{} entries seen, {} new article(s) extracted",
                            total, new
                        ))
                    },
                    stop,
                )
                .await;
            });
        }
        {
            let state = state.clone();
            let stop = stop.clone();
            let lock = purge_lock.clone();
            tokio::spawn(async move {
                worker_loop(
                    purge_q.clone(),
                    state,
                    lock,
                    "purge",
                    |s| async move {
                        let n = run_purge(s).await?;
                        Ok(format!("{} article(s) removed", n))
                    },
                    stop,
                )
                .await;
            });
        }

        // Kick off one immediate run of each so a fresh start has a warm
        // cache and an honored TTL without waiting for the first cron tick.
        let refresh_q2 = db.queue(REFRESH_QUEUE_NAME, honker::QueueOpts::default());
        let purge_q2 = db.queue(PURGE_QUEUE_NAME, honker::QueueOpts::default());
        tokio::task::spawn_blocking(move || {
            let _ = refresh_q2.enqueue(
                &serde_json::json!({"trigger": "startup"}),
                honker::EnqueueOpts::default(),
            );
            let _ = purge_q2.enqueue(
                &serde_json::json!({"trigger": "startup"}),
                honker::EnqueueOpts::default(),
            );
        });
    } else {
        tracing::info!("no [scheduler] block in config — running without background jobs");
    }

    // Optional Gemini server. Failure to start it is logged but does
    // not block the HTTP reader from coming up.
    if let Some(gem_cfg) = config.gemini.clone() {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = gemini::serve(state, gem_cfg).await {
                tracing::error!("gemini server exited: {e:#}");
            }
        });
    }

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5050);
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("listening on http://0.0.0.0:{}", port);

    let serve = axum::serve(listener, app);
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("ctrl-c — stopping scheduler / workers");
        stop.store(true, Ordering::Relaxed);
    };
    tokio::select! {
        r = serve => r?,
        _ = shutdown => {}
    }
    Ok(())
}
