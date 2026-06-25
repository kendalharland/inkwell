//! Process-global state shared across routes and background jobs.
//!
//! Held inside an `Arc<AppState>` so handlers, the worker tasks, and the
//! scheduler thread can all hand it around cheaply.

use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

use feed_rs::model::Feed as ParsedFeed;
use tokio::sync::{Mutex, RwLock};

/// One configured group. `feed_indices` point into `AppState::feeds` —
/// indices, not URL strings, so a feed listed in multiple groups still
/// maps to one cache entry.
#[derive(Clone)]
pub struct GroupInfo {
    pub name: String,
    pub feed_indices: Vec<usize>,
}

/// In-memory snapshot of a parsed feed and when it was fetched. The TTL
/// check (in `feeds::ensure_feeds`) reads `fetched_at` to decide whether
/// to refresh.
pub struct CachedFeed {
    pub parsed: ParsedFeed,
    pub fetched_at: SystemTime,
}

pub struct AppState {
    /// Deduplicated list of feed URLs. Group-to-feed mapping is by index
    /// into this vector. Mutable to support the admin UI (#3) — wrapped
    /// in `RwLock` so live mutations can hot-swap the list without
    /// restarting the server.
    pub feeds: RwLock<Vec<String>>,
    /// Display titles parsed from the feed payload (Atom/RSS `<title>`).
    /// Populated lazily as feeds are fetched. `None` slots fall back to
    /// the URL in `feeds`. Aligned 1:1 with `feeds`.
    pub feed_titles: RwLock<Vec<Option<String>>>,
    /// Groups carry indices into `feeds`. Mutable alongside `feeds`; the
    /// admin reload-helper rebuilds both atomically.
    pub groups: RwLock<Vec<GroupInfo>>,
    pub http: reqwest::Client,
    /// Separate client used by the feed-discovery autocomplete. Built
    /// with `redirect(Policy::none())` so [`crate::feed_search`] can
    /// follow redirects manually and re-check each hop against the
    /// SSRF block-list. Do not use this client for anything that
    /// expects automatic redirect handling (e.g. article extraction).
    pub discovery_http: reqwest::Client,
    /// Parsed-feed cache keyed by feed index. Eviction is purely TTL-based.
    pub feed_cache: RwLock<HashMap<usize, CachedFeed>>,
    /// Connection used for the article cache. Honker has its own connection
    /// to the same SQLite file; we both rely on WAL for concurrent reads.
    pub db: Mutex<rusqlite::Connection>,
    pub feed_ttl: Duration,
    /// Pre-computed `article_ttl_days * 86400` so the purge job can do
    /// arithmetic without re-reading config.
    pub article_ttl_secs: i64,
    /// Default for the compact-density UI when neither a cookie nor a
    /// query-string override is present on the request.
    pub compact_default: bool,
    /// Default for the dark-theme variant. Same precedence rules as
    /// `compact_default`.
    pub dark_default: bool,
    /// Feed-discovery providers, copied off the config at startup.
    /// Read by the admin autocomplete endpoint to know which third
    /// parties to fan out to.
    pub feed_search: crate::config::FeedSearchConfig,
}

