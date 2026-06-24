//! Admin: mutation primitives for the YAML config plus a hot-reload helper.
//!
//! Persistence model — kept deliberately simple:
//!
//! 1. The admin UI reads the current YAML off disk and parses it into
//!    [`ConfigFile`].
//! 2. A mutation primitive (`add_feed`, `remove_group`, etc.) modifies
//!    the parsed value in memory.
//! 3. The result is serialized back to the same path.
//! 4. [`apply_config`] rebuilds the live runtime state under the
//!    `AppState` write-locks and clears the feed cache.
//!
//! Tradeoffs we accept:
//!
//! * Comments in the YAML are lost on rewrite (serde_yml strips them).
//!   The admin UI warns about this; users who care can edit the file
//!   directly and the server picks it up on next restart.
//! * The feed-cache clear means the next request triggers a refetch.
//!   That's the right behavior because feed indices may have shifted
//!   (e.g. a feed was removed from group A but still exists via group B
//!   — its index in the deduplicated `feeds` vec changes).
//!
//! Mutation primitives are pure (operate on `&mut ConfigFile`) and
//! unit-tested in isolation. The thin IO wrapper `mutate_config` joins
//! them with reading, writing, and reloading.

use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::{
    config::{ConfigFile, GroupConfig},
    state::{AppState, GroupInfo},
};

/// Project a parsed config into the index-keyed runtime layout that
/// handlers and jobs read from. Used both at startup and after every
/// admin write.
pub fn build_runtime_state(cfg: &ConfigFile) -> (Vec<String>, Vec<Option<String>>, Vec<GroupInfo>) {
    let mut feeds: Vec<String> = Vec::new();
    let mut url_to_idx: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<GroupInfo> = Vec::new();
    for g in &cfg.rss.groups {
        let mut idxs = Vec::new();
        for url in &g.feeds {
            let idx = *url_to_idx.entry(url.clone()).or_insert_with(|| {
                let i = feeds.len();
                feeds.push(url.clone());
                i
            });
            idxs.push(idx);
        }
        groups.push(GroupInfo {
            name: g.name.clone(),
            feed_indices: idxs,
        });
    }
    let titles = vec![None; feeds.len()];
    (feeds, titles, groups)
}

/// Atomically swap the live runtime state to match `cfg`. The feed cache
/// is cleared because feed indices may have shifted and stale entries
/// would be served against the wrong feed.
pub async fn apply_config(state: &AppState, cfg: &ConfigFile) {
    let (feeds, titles, groups) = build_runtime_state(cfg);
    *state.feeds.write().await = feeds;
    *state.feed_titles.write().await = titles;
    *state.groups.write().await = groups;
    state.feed_cache.write().await.clear();
}

pub fn read_config(state: &AppState) -> Result<ConfigFile> {
    let yaml = std::fs::read_to_string(&state.config_path)
        .with_context(|| format!("reading {}", state.config_path.display()))?;
    serde_yml::from_str(&yaml).context("parsing yaml")
}

pub fn write_config(state: &AppState, cfg: &ConfigFile) -> Result<()> {
    let yaml = serde_yml::to_string(cfg).context("serializing yaml")?;
    std::fs::write(&state.config_path, yaml)
        .with_context(|| format!("writing {}", state.config_path.display()))
}

/// Read → mutate → write → hot-reload, in that order. The closure is
/// the only piece a route handler has to supply.
pub async fn mutate_config<F>(state: &AppState, f: F) -> Result<()>
where
    F: FnOnce(&mut ConfigFile) -> Result<()>,
{
    let mut cfg = read_config(state)?;
    f(&mut cfg)?;
    write_config(state, &cfg)?;
    apply_config(state, &cfg).await;
    Ok(())
}

// ----- Mutation primitives ----------------------------------------------

pub fn add_feed_to_group(cfg: &mut ConfigFile, group: &str, url: &str) -> Result<()> {
    let url = url.trim();
    if url.is_empty() {
        anyhow::bail!("url is empty");
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        anyhow::bail!("url must start with http:// or https://");
    }
    let Some(g) = cfg.rss.groups.iter_mut().find(|g| g.name == group) else {
        anyhow::bail!("group {:?} does not exist", group);
    };
    if !g.feeds.iter().any(|u| u == url) {
        g.feeds.push(url.to_string());
    }
    Ok(())
}

pub fn remove_feed_from_group(cfg: &mut ConfigFile, group: &str, url: &str) -> Result<()> {
    let Some(g) = cfg.rss.groups.iter_mut().find(|g| g.name == group) else {
        anyhow::bail!("group {:?} does not exist", group);
    };
    g.feeds.retain(|u| u != url);
    Ok(())
}

pub fn add_group(cfg: &mut ConfigFile, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("group name is empty");
    }
    if cfg.rss.groups.iter().any(|g| g.name == name) {
        anyhow::bail!("group {:?} already exists", name);
    }
    cfg.rss.groups.push(GroupConfig {
        name: name.to_string(),
        feeds: Vec::new(),
    });
    Ok(())
}

pub fn remove_group(cfg: &mut ConfigFile, name: &str) -> Result<()> {
    let before = cfg.rss.groups.len();
    cfg.rss.groups.retain(|g| g.name != name);
    if cfg.rss.groups.len() == before {
        anyhow::bail!("group {:?} does not exist", name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RssConfig, ViewConfig};

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

    #[test]
    fn add_group_creates_empty_group() {
        let mut c = empty_config();
        add_group(&mut c, "tech").unwrap();
        assert_eq!(c.rss.groups.len(), 1);
        assert_eq!(c.rss.groups[0].name, "tech");
        assert!(c.rss.groups[0].feeds.is_empty());
    }

    #[test]
    fn add_group_rejects_duplicate() {
        let mut c = config_with(&[("tech", &[])]);
        let err = add_group(&mut c, "tech").unwrap_err().to_string();
        assert!(err.contains("already exists"));
    }

    #[test]
    fn add_group_rejects_empty_name() {
        let mut c = empty_config();
        assert!(add_group(&mut c, "   ").is_err());
    }

    #[test]
    fn remove_group_deletes() {
        let mut c = config_with(&[("a", &["https://a"]), ("b", &[])]);
        remove_group(&mut c, "a").unwrap();
        assert_eq!(c.rss.groups.len(), 1);
        assert_eq!(c.rss.groups[0].name, "b");
    }

    #[test]
    fn remove_group_errors_when_missing() {
        let mut c = config_with(&[("a", &[])]);
        assert!(remove_group(&mut c, "missing").is_err());
    }

    #[test]
    fn add_feed_appends_to_group() {
        let mut c = config_with(&[("tech", &[])]);
        add_feed_to_group(&mut c, "tech", "https://hn.org/rss").unwrap();
        assert_eq!(c.rss.groups[0].feeds, vec!["https://hn.org/rss"]);
    }

    #[test]
    fn add_feed_is_idempotent() {
        let mut c = config_with(&[("tech", &["https://hn.org/rss"])]);
        add_feed_to_group(&mut c, "tech", "https://hn.org/rss").unwrap();
        assert_eq!(c.rss.groups[0].feeds.len(), 1);
    }

    #[test]
    fn add_feed_rejects_missing_group() {
        let mut c = empty_config();
        assert!(add_feed_to_group(&mut c, "x", "https://a").is_err());
    }

    #[test]
    fn add_feed_rejects_non_http_url() {
        let mut c = config_with(&[("tech", &[])]);
        assert!(add_feed_to_group(&mut c, "tech", "ftp://x").is_err());
        assert!(add_feed_to_group(&mut c, "tech", "javascript:alert(1)").is_err());
        assert!(add_feed_to_group(&mut c, "tech", "").is_err());
    }

    #[test]
    fn add_feed_trims_whitespace() {
        let mut c = config_with(&[("tech", &[])]);
        add_feed_to_group(&mut c, "tech", "  https://x.com/rss  ").unwrap();
        assert_eq!(c.rss.groups[0].feeds, vec!["https://x.com/rss"]);
    }

    #[test]
    fn remove_feed_drops_url() {
        let mut c = config_with(&[("tech", &["https://a", "https://b"])]);
        remove_feed_from_group(&mut c, "tech", "https://a").unwrap();
        assert_eq!(c.rss.groups[0].feeds, vec!["https://b"]);
    }

    #[test]
    fn remove_feed_is_silent_when_missing() {
        let mut c = config_with(&[("tech", &["https://a"])]);
        remove_feed_from_group(&mut c, "tech", "https://missing").unwrap();
        assert_eq!(c.rss.groups[0].feeds, vec!["https://a"]);
    }

    #[test]
    fn build_runtime_state_dedupes_feeds_across_groups() {
        let c = config_with(&[
            ("a", &["https://shared", "https://only-a"]),
            ("b", &["https://shared", "https://only-b"]),
        ]);
        let (feeds, titles, groups) = build_runtime_state(&c);
        assert_eq!(feeds.len(), 3, "shared feed counts once");
        assert_eq!(titles.len(), 3);
        assert_eq!(groups[0].feed_indices.len(), 2);
        assert_eq!(groups[1].feed_indices.len(), 2);
        // Group "b" must reference the same index as group "a" for the shared URL.
        assert_eq!(groups[0].feed_indices[0], groups[1].feed_indices[0]);
    }
}
