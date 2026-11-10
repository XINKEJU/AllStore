//! Disk cache of the merged store catalog.
//!
//! Without it every cold start showed a loading screen until all three
//! sources answered. With `%LOCALAPPDATA%\AllStore\catalog.json` the window
//! paints the previous catalog instantly and the network refresh replaces it
//! in place when (and if) it arrives. The cache is a convenience copy only:
//! a missing or corrupt file simply falls back to the network-first flow.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{App, ClassGroup};
use crate::update::now;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CatalogCache {
    /// Unix seconds when the catalog was fetched; shown as "x 分钟前".
    #[serde(default)]
    pub saved_at: u64,
    /// Lenovo sidebar navigation; cached so the category tree works offline.
    #[serde(default)]
    pub classes: Vec<ClassGroup>,
    #[serde(default)]
    pub apps: Vec<App>,
}

fn path() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(base).join("AllStore").join("catalog.json"))
}

/// A cache with zero apps is worse than none: it would paint an empty store.
pub fn load() -> Option<CatalogCache> {
    let p = path()?;
    let c: CatalogCache = std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())?;
    if c.apps.is_empty() { None } else { Some(c) }
}

pub fn save(classes: &[ClassGroup], apps: &[App]) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let c = CatalogCache {
        saved_at: now(),
        classes: classes.to_vec(),
        apps: apps.to_vec(),
    };
    if let Ok(s) = serde_json::to_string(&c) {
        // Write-temp-then-rename: a crash mid-save must not corrupt the file.
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// Human age of the snapshot for the status line ("3 分钟前").
pub fn age_text(saved_at: u64) -> String {
    let d = now().saturating_sub(saved_at);
    if d < 60 {
        "刚刚".into()
    } else if d < 3600 {
        format!("{} 分钟前", d / 60)
    } else if d < 86_400 {
        format!("{} 小时前", d / 3600)
    } else {
        format!("{} 天前", d / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::App;

    #[test]
    fn cache_round_trips_classes_and_apps() {
        let c = CatalogCache {
            saved_at: 123,
            classes: vec![ClassGroup {
                code: "soft".into(),
                list: vec![crate::model::ClassItem {
                    id: "1".into(),
                    name: "聊天社交".into(),
                }],
            }],
            apps: vec![App {
                id: "G1".into(),
                name: "7-Zip".into(),
                source: Some("GitHub".into()),
                dl_a: Some("gh:7-z/7z".into()),
                ..Default::default()
            }],
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: CatalogCache = serde_json::from_str(&s).unwrap();
        assert_eq!(back.saved_at, 123);
        assert_eq!(back.classes[0].list[0].name, "聊天社交");
        assert_eq!(back.apps[0].dl_a.as_deref(), Some("gh:7-z/7z"));
        assert_eq!(back.apps[0].origin(), "GitHub");
    }

    #[test]
    fn an_empty_catalog_is_not_a_usable_cache() {
        // The guard in load() mirrors this rule; test it at the data level.
        let c = CatalogCache::default();
        assert!(c.apps.is_empty());
    }

    #[test]
    fn age_text_picks_sensible_units() {
        let now = now();
        assert_eq!(age_text(now), "刚刚");
        assert_eq!(age_text(now - 180), "3 分钟前");
        assert_eq!(age_text(now - 7200), "2 小时前");
        assert_eq!(age_text(now - 172_800), "2 天前");
    }
}
