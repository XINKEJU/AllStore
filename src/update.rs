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
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_path() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(base).join("MindosShop").join("updates.json"))
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

/// Pull a broad catalog using only whitelisted endpoints. A failed category
/// is skipped instead of aborting the whole check.
fn catalog(token: &str, progress: &mut dyn FnMut(usize, usize)) -> Result<Vec<App>, String> {
    let groups = net::classes(token)?;
    let mut targets: Vec<(String, String)> = vec![("soft".into(), "-1".into())];
    if let Some(g) = groups.iter().find(|g| g.code == "soft") {
        for c in g.list.iter().take(6) {
            if c.name == "全部" || c.id.is_empty() {
                continue;
            }
            targets.push((g.code.clone(), c.id.clone()));
        }
    }
    if targets.len() == 1 {
        for g in groups.iter().take(4) {
            targets.push((g.code.clone(), "-1".into()));
        }
    }

    let total = targets.len();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut failures = 0usize;
    for (i, (code, id)) in targets.iter().enumerate() {
        progress(i, total);
        match net::class_apps(token, code, id, 60) {
            Ok(list) => {
                for a in list {
                    if !a.id.is_empty() && seen.insert(a.id.clone()) {
                        out.push(a);
                    }
                }
            }
            Err(_) => failures += 1,
        }
    }
    progress(total, total);
    if out.is_empty() && failures > 0 {
        return Err("无法获取应用目录".into());
    }
    Ok(out)
}

pub fn check(
    token: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Vec<UpdateItem>, String> {
    let apps = catalog(token, progress)?;
    let local = installed::scan();
    Ok(match_updates(&apps, &local))
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

    /// Diagnostic for the real update check. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires live network"]
    fn live_catalog_matches_local_inventory() {
        let token = net::register().expect("token registration");
        let mut progress = |_, _| {};
        let items = check(&token, &mut progress).expect("update check");
        println!("matched {} updatable apps", items.len());
        for u in items.iter().take(12) {
            println!("  {} : {} -> {}", u.name, u.current, u.latest);
        }
    }
}
