//! YAML configuration types.
//!
//! The config is loaded once at startup and never re-read; changes require a
//! restart. This is intentional: the scheduler reconciliation logic
//! (`crate::jobs::reconcile_schedules`) runs from `main`, and silent live
//! reload would mean schedule changes apply at non-obvious times.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConfigFile {
    pub rss: RssConfig,
    /// Optional. When absent the server runs as a foreground reader with no
    /// background refresh or purge. Useful for ad-hoc / one-shot use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler: Option<SchedulerConfig>,
    /// Optional. Layout/density preferences for the reader UI.
    #[serde(default, skip_serializing_if = "ViewConfig::is_default")]
    pub view: ViewConfig,
    /// Optional. When absent, no Gemini server is started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini: Option<GeminiConfig>,
    /// Feed-discovery providers used by the admin UI's autocomplete.
    /// Defaults to feedsearch.dev (no API key) so the feature works out
    /// of the box; extra providers (Feedly, Inoreader, FeedBagel) slot
    /// in here when their keys are available.
    #[serde(default)]
    pub feed_search: FeedSearchConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FeedSearchConfig {
    #[serde(default = "default_feed_search_providers")]
    pub providers: Vec<FeedSearchProvider>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FeedSearchProvider {
    /// Built-in: GET the user's URL, parse `<link rel="alternate">`
    /// tags that point to RSS/Atom/JSON-feed payloads, return them.
    /// Works for any site that advertises its feeds in markup —
    /// which is most of them, and notably doesn't depend on a third
    /// party staying up.
    LinkAutoDiscovery,
    /// https://feedsearch.dev — open, no API key. Currently behind a
    /// Cloudflare challenge that blocks server-side requests, so this
    /// provider often returns nothing; left available for environments
    /// that can reach it (Cloudflare allow-lists, etc.).
    Feedsearch,
    // Slots for future providers; left intentionally absent until we
    // wire keyed flows. Adding a new variant + matching arm in
    // `feed_search::search_all` is the only code needed to enable one.
}

fn default_feed_search_providers() -> Vec<FeedSearchProvider> {
    vec![FeedSearchProvider::LinkAutoDiscovery]
}

impl Default for FeedSearchConfig {
    fn default() -> Self {
        Self {
            providers: default_feed_search_providers(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GeminiConfig {
    /// Bind address, e.g. "0.0.0.0:1965" (the Gemini default port).
    pub bind: String,
    /// PEM path for the TLS certificate. Generated on first launch if
    /// missing — Gemini uses TOFU, so once a client trusts the cert
    /// keep these files stable across restarts.
    pub cert_pem: PathBuf,
    /// PEM path for the TLS private key. Generated alongside `cert_pem`.
    pub key_pem: PathBuf,
    /// Names embedded as Subject Alternative Names in a freshly generated
    /// certificate. Include every hostname or IP a client might use to
    /// reach the server (e.g. `localhost`, your LAN IP, your mDNS name).
    #[serde(default = "default_san")]
    pub hostnames: Vec<String>,
}

fn default_san() -> Vec<String> {
    vec!["localhost".into()]
}

#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq)]
pub struct ViewConfig {
    /// When `true`, the reader renders the compact (denser) layout by
    /// default. Users can override per-session via the nav toggle, which
    /// stores their choice in a cookie.
    #[serde(default)]
    pub compact_default: bool,
    /// When `true`, new visitors see the dark theme. Users can toggle
    /// via the nav link; the choice persists in a cookie.
    #[serde(default)]
    pub dark_default: bool,
}

impl ViewConfig {
    fn is_default(&self) -> bool {
        *self == ViewConfig::default()
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RssConfig {
    pub groups: Vec<GroupConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GroupConfig {
    pub name: String,
    pub feeds: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SchedulerConfig {
    /// Cron expression for the feed-refresh + pre-extract job.
    pub refresh: String,
    /// Cron expression for the article-purge job.
    pub purge: String,
    /// Articles whose `fetched_at` is older than this are deleted by the
    /// purge job. A fresh re-tap then re-extracts the live URL.
    pub article_ttl_days: u32,
    #[serde(default = "default_log_file")]
    pub log_file: PathBuf,
}

fn default_log_file() -> PathBuf {
    PathBuf::from("./inkwell.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_without_scheduler() {
        let yaml = r#"
rss:
  groups:
    - name: "tech"
      feeds:
        - https://example.com/a.xml
"#;
        let c: ConfigFile = serde_yml::from_str(yaml).unwrap();
        assert_eq!(c.rss.groups.len(), 1);
        assert_eq!(c.rss.groups[0].name, "tech");
        assert_eq!(c.rss.groups[0].feeds, vec!["https://example.com/a.xml"]);
        assert!(c.scheduler.is_none());
    }

    #[test]
    fn parses_full_config_with_scheduler() {
        let yaml = r#"
rss:
  groups:
    - name: "tech"
      feeds: [https://a, https://b]
    - name: "world"
      feeds: [https://c]
scheduler:
  refresh: "@every 10m"
  purge: "0 3 * * *"
  article_ttl_days: 30
  log_file: ./other.log
"#;
        let c: ConfigFile = serde_yml::from_str(yaml).unwrap();
        assert_eq!(c.rss.groups.len(), 2);
        let s = c.scheduler.unwrap();
        assert_eq!(s.refresh, "@every 10m");
        assert_eq!(s.purge, "0 3 * * *");
        assert_eq!(s.article_ttl_days, 30);
        assert_eq!(s.log_file, PathBuf::from("./other.log"));
    }

    #[test]
    fn scheduler_log_file_defaults() {
        let yaml = r#"
rss: { groups: [ { name: t, feeds: [https://a] } ] }
scheduler:
  refresh: "@every 1m"
  purge: "@every 1h"
  article_ttl_days: 7
"#;
        let c: ConfigFile = serde_yml::from_str(yaml).unwrap();
        assert_eq!(
            c.scheduler.unwrap().log_file,
            PathBuf::from("./inkwell.log")
        );
    }

    #[test]
    fn rejects_missing_required_scheduler_fields() {
        let yaml = r#"
rss: { groups: [ { name: t, feeds: [https://a] } ] }
scheduler: { refresh: "@every 1m" }
"#;
        assert!(serde_yml::from_str::<ConfigFile>(yaml).is_err());
    }
}
