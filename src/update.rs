//! Update detection without leaking the local software inventory.
//!
//! The store catalog is fetched through the already-whitelisted endpoints,
//! then matched against the registry inventory entirely on this machine.
//! No local application name is ever sent to the server.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::installed::{self, Installed};
use crate::model::App;
use crate::net;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateItem {
    pub soft_id: String,
    pub name: String,
    pub current: String,
    pub latest: String,
    pub size: Option<String>,
    pub logo: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Cache {
    pub checked_at: u64,
    pub items: Vec<UpdateItem>,
    pub ignored: Vec<String>,
    /// Catalog snapshot. Keeping it lets the app re-match the local registry
    /// after an install without re-downloading the whole store catalog.
    #[serde(default)]
    pub catalog: Vec<App>,
    /// Display copies of the ignored rows. `ignored` stays the source of
    /// truth; this only lets the "ignored updates" page show names offline.
    /// `default` keeps caches written by 0.6.0 and earlier readable.
    #[serde(default)]
    pub ignored_items: Vec<UpdateItem>,
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub catalog: Vec<App>,
    pub items: Vec<UpdateItem>,
    /// Same local snapshot used for the comparison, so the store page can
    /// refresh its "installed" badges without a second registry walk.
    pub local: Vec<Installed>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_path() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(base).join("AllStore").join("updates.json"))
}

pub fn load_cache() -> Cache {
    let Some(p) = cache_path() else {
        return Cache::default();
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Cache>(&s).ok())
        .unwrap_or_default()
}

pub fn save_cache(cache: &Cache) {
    let Some(p) = cache_path() else {
        return;
    };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(cache) {
        let _ = std::fs::write(p, s);
    }
}

/// Pull a broad catalog from every enabled source. A failed source is skipped
/// (its warning is logged) instead of aborting the whole check, so the store
/// still opens when one feed is down.
fn catalog(token: &str, progress: &mut dyn FnMut(usize, usize)) -> Result<Vec<App>, String> {
    let mut quiet = |_: &str| {};
    let merged = crate::source::catalog(token, progress, &mut quiet);
    net::log(&format!(
        "CATALOG merged={} sources={:?}",
        merged.apps.len(),
        merged.sources
    ));
    for w in &merged.warnings {
        net::log(&format!("SOURCE warn {}", w));
    }
    if merged.apps.is_empty() {
        return Err(if merged.warnings.is_empty() {
            "无法获取应用目录".to_string()
        } else {
            merged.warnings.join("；")
        });
    }
    Ok(merged.apps)
}

pub fn check(token: &str, progress: &mut dyn FnMut(usize, usize)) -> Result<CheckResult, String> {
    let catalog = catalog(token, progress)?;
    let local = installed::scan();
    Ok(CheckResult {
        items: match_updates(&catalog, &local),
        catalog,
        local,
    })
}

/// Re-read the local uninstall keys and compare them with a cached catalog.
/// This is what makes the "N updates" badge drop right after an installer
/// finishes — the old code only ever refreshed the list on a full network
/// check, so a freshly updated app kept showing as outdated for hours.
pub fn rescan_with(catalog: &[App], local: &[Installed], ignored: &[String]) -> Vec<UpdateItem> {
    match_updates(catalog, local)
        .into_iter()
        .filter(|i| !ignored.contains(&i.soft_id))
        .collect()
}

/// Pure function so the matching rules can be exercised without network or
/// registry access.
pub fn match_updates(catalog: &[App], local: &[Installed]) -> Vec<UpdateItem> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for l in local {
        // The catalog can contain several entries for one product; pick the
        // highest version instead of whichever happens to come first.
        let mut best: Option<&App> = None;
        for a in catalog {
            let Some(v) = a.version.as_deref() else {
                continue;
            };
            if v.trim().is_empty() || !installed::name_matches(&a.name, &l.name) {
                continue;
            }
            let better = best
                .and_then(|b| b.version.as_deref())
                .is_none_or(|old| installed::is_newer(v, old));
            if better {
                best = Some(a);
            }
        }
        let Some(a) = best else {
            continue;
        };
        let latest = a.version.clone().unwrap_or_default();
        if !installed::is_newer(&latest, &l.version) || !seen.insert(a.id.clone()) {
            continue;
        }
        out.push(UpdateItem {
            soft_id: a.id.clone(),
            name: a.name.clone(),
            current: l.version.clone(),
            latest,
            size: a.size.clone(),
            logo: a.logo_url().map(str::to_string),
            url: a.download_url(),
        });
    }
    out.sort_by(|x, y| x.name.cmp(&y.name));
    out
}

/// Restore one ignored update. Pure so the "restore" path is unit-testable
/// without a window; the caller re-scans afterwards to bring the row back.
pub fn unignore(
    ignored: &mut Vec<String>,
    items: &mut Vec<UpdateItem>,
    index: usize,
) -> Option<UpdateItem> {
    if index >= ignored.len() {
        return None;
    }
    let id = ignored.remove(index);
    let entry = items
        .iter()
        .position(|x| x.soft_id == id)
        .map(|p| items.remove(p))
        .unwrap_or(UpdateItem {
            soft_id: id.clone(),
            name: format!("应用 {}", id),
            current: String::new(),
            latest: String::new(),
            size: None,
            logo: None,
            url: String::new(),
        });
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installed::Installed;

    fn app(name: &str, ver: &str, id: &str) -> App {
        App {
            id: id.into(),
            name: name.into(),
            version: Some(ver.into()),
            ..Default::default()
        }
    }

    fn installed(name: &str, ver: &str) -> Installed {
        Installed {
            name: name.into(),
            version: ver.into(),
            ..Default::default()
        }
    }

    #[test]
    fn detects_only_newer_versions() {
        let catalog = vec![
            app("微信 Windows 版", "3.9.10", "1"),
            app("Chrome", "120.0", "2"),
        ];
        let local = vec![
            installed("微信", "3.9.8"),
            installed("Chrome", "120.0"),
            installed("商店里没有的软件", "1.0"),
        ];
        let out = match_updates(&catalog, &local);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].soft_id, "1");
        assert_eq!(out[0].current, "3.9.8");
        assert_eq!(out[0].latest, "3.9.10");
    }

    #[test]
    fn skips_apps_without_a_version() {
        let catalog = vec![app("微信", "", "1")];
        let local = vec![installed("微信", "1.0")];
        assert!(match_updates(&catalog, &local).is_empty());
    }

    #[test]
    fn picks_the_newest_catalog_entry() {
        let catalog = vec![app("QQ", "9.9.35", "old"), app("QQ", "9.9.37", "new")];
        let local = vec![installed("QQ", "9.9.30")];
        let out = match_updates(&catalog, &local);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].soft_id, "new");
        assert_eq!(out[0].latest, "9.9.37");
    }

    #[test]
    fn rescan_drops_an_app_once_the_installed_version_catches_up() {
        let catalog = vec![app("微信", "3.9.10", "1"), app("QQ", "9.9.37", "2")];
        let before = vec![installed("微信", "3.9.8"), installed("QQ", "9.9.30")];
        assert_eq!(rescan_with(&catalog, &before, &[]).len(), 2);

        // The user ran the installer: only the registry version changed.
        let after = vec![installed("微信", "3.9.10"), installed("QQ", "9.9.30")];
        let items = rescan_with(&catalog, &after, &[]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].soft_id, "2");

        let ignored = vec!["2".to_string()];
        assert!(rescan_with(&catalog, &after, &ignored).is_empty());
    }

    #[test]
    fn unignore_removes_the_id_and_its_display_row() {
        let mut ignored = vec!["1".to_string(), "2".to_string()];
        let mut items = vec![
            UpdateItem {
                soft_id: "1".into(),
                name: "微信".into(),
                current: "3.9.8".into(),
                latest: "3.9.10".into(),
                size: None,
                logo: None,
                url: String::new(),
            },
            UpdateItem {
                soft_id: "2".into(),
                name: "QQ".into(),
                current: "9.9.30".into(),
                latest: "9.9.37".into(),
                size: None,
                logo: None,
                url: String::new(),
            },
        ];
        let restored = unignore(&mut ignored, &mut items, 1).unwrap();
        assert_eq!(restored.name, "QQ");
        assert_eq!(ignored, vec!["1".to_string()]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].soft_id, "1");
        assert!(unignore(&mut ignored, &mut items, 9).is_none());
        // A legacy cache may have an id without a display row.
        let legacy = unignore(&mut ignored, &mut items, 0).unwrap();
        assert_eq!(legacy.soft_id, "1");
        assert!(ignored.is_empty());
    }

    #[test]
    fn old_cache_without_ignored_items_still_loads() {
        let json = r#"{"checked_at":123,"items":[],"ignored":["7"],"catalog":[]}"#;
        let c: Cache = serde_json::from_str(json).unwrap();
        assert_eq!(c.checked_at, 123);
        assert_eq!(c.ignored, vec!["7".to_string()]);
        assert!(c.ignored_items.is_empty());
        assert!(c.catalog.is_empty());
    }

    /// Diagnostic for the real update check. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires live network"]
    fn live_catalog_matches_local_inventory() {
        let token = net::register().expect("token registration");
        let mut progress = |_, _| {};
        let result = check(&token, &mut progress).expect("update check");
        println!("matched {} updatable apps", result.items.len());
        for u in result.items.iter().take(12) {
            println!("  {} : {} -> {}", u.name, u.current, u.latest);
        }
    }
}
