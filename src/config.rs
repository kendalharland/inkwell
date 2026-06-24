//! YAML configuration types.
//!
//! The config is loaded once at startup and never re-read; changes require a
//! restart. This is intentional: the scheduler reconciliation logic
//! (`crate::jobs::reconcile_schedules`) runs from `main`, and silent live
//! reload would mean schedule changes apply at non-obvious times.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ConfigFile {
    pub rss: RssConfig,
    /// Optional. When absent the server runs as a foreground reader with no
    /// background refresh or purge. Useful for ad-hoc / one-shot use.
    #[serde(default)]
    pub scheduler: Option<SchedulerConfig>,
    /// Optional. Layout/density preferences for the reader UI.
    #[serde(default)]
    pub view: ViewConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ViewConfig {
    /// When `true`, the reader renders the compact (denser) layout by
    /// default. Users can override per-session via the nav toggle, which
    /// stores their choice in a cookie.
    #[serde(default)]
    pub compact_default: bool,
}

#[derive(Debug, Deserialize)]
pub struct RssConfig {
    pub groups: Vec<GroupConfig>,
}

#[derive(Debug, Deserialize)]
pub struct GroupConfig {
    pub name: String,
    pub feeds: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
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
        assert_eq!(c.scheduler.unwrap().log_file, PathBuf::from("./inkwell.log"));
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
