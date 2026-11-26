//! Update detection without leaking the local software inventory.
//!
//! The store catalog is fetched through the already-whitelisted endpoints,
//! then matched against the registry inventory entirely on this machine.
//! No local application name is ever sent to the server.

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
    /// Digest advertised by the source, if any. Absent in older caches.
    #[serde(default)]
    pub hash: Option<String>,
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
///
/// Two directions of ambiguity are resolved here, and both used to produce a
/// wrong list:
///
/// * one catalog row can match several uninstall keys (`Git` next to a stale
///   `GitHub 1.0` entry, `Google Chrome` next to `Google Chrome Canary`) — the
///   highest local version wins, otherwise the leftover key reports the app as
///   forever outdated;
/// * one local app can match several catalog rows (`微信` and `微信电脑版`, a
///   Tencent 3.9 entry next to a Lenovo 4.2.1.9) — only the newest catalog row
///   is kept, otherwise the same product shows up twice.
pub fn match_updates(catalog: &[App], local: &[Installed]) -> Vec<UpdateItem> {
    // Normalizing both names on every comparison would allocate ~255k short
    // strings for 150 installed apps × 850 catalog rows on the UI thread each
    // time the update list is rebuilt. Normalize once per row instead.
    let local_norm: Vec<String> = local
        .iter()
        .map(|l| installed::normalize(&l.name))
        .collect();
    let catalog_norm: Vec<String> = catalog
        .iter()
        .map(|a| installed::normalize(&a.name))
        .collect();

    // Best catalog row for each local app: the highest version among the rows
    // whose name matches.
    let mut best_for_local: Vec<Option<usize>> = vec![None; local.len()];
    for (li, l) in local.iter().enumerate() {
        if l.version.trim().is_empty() {
            continue;
        }
        let mut best: Option<usize> = None;
        for (ci, a) in catalog.iter().enumerate() {
            let Some(v) = a.version.as_deref() else {
                continue;
            };
            if v.trim().is_empty()
                || !installed::matches_normalized(&catalog_norm[ci], &local_norm[li])
            {
                continue;
            }
            let better = match best.and_then(|b| catalog[b].version.as_deref()) {
                Some(old) => installed::is_newer(v, old),
                None => true,
            };
            if better {
                best = Some(ci);
            }
        }
        best_for_local[li] = best;
    }

    // Group by catalog row, keeping the newest local version seen for it.
    struct Group<'a> {
        row: usize,
        current: &'a str,
    }
    let mut groups: Vec<Group> = Vec::new();
    for (li, picked) in best_for_local.iter().enumerate() {
        let Some(ci) = *picked else { continue };
        let version = local[li].version.trim();
        match groups.iter_mut().find(|g| g.row == ci) {
            Some(g) => {
                if installed::is_newer(version, g.current) {
                    g.current = version;
                }
            }
            None => groups.push(Group {
                row: ci,
                current: version,
            }),
        }
    }

    let mut out = Vec::new();
    for g in groups {
        let a = &catalog[g.row];
        let Some(latest) = a.version.clone() else {
            continue;
        };
        if !installed::is_newer(&latest, g.current) {
            continue;
        }
        out.push(UpdateItem {
            soft_id: a.id.clone(),
            name: a.name.clone(),
            current: g.current.to_string(),
            latest,
            size: a.size.clone(),
            logo: a.logo_url().map(str::to_string),
            url: a.download_url(),
            hash: a.hash.clone(),
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
            hash: None,
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
    fn an_app_installed_later_joins_the_next_local_match() {
        // The cached catalog is reused for hours; what changes between two
        // rescans is the machine. A freshly installed app with an older build
        // than the catalog offers has to appear, and an up-to-date one must
        // not.
        let catalog = vec![
            app("OBS Studio", "33.0.3", "obs"),
            app("Kimi", "3.3.0", "kimi"),
            app("Nmap", "7.991", "nmap"),
        ];
        let before = vec![
            installed("OBS Studio", "33.0.3"),
            installed("Nmap", "7.991"),
        ];
        assert!(match_updates(&catalog, &before).is_empty());

        let after = vec![
            installed("OBS Studio", "33.0.3"),
            installed("Nmap", "7.991"),
            installed("Kimi", "3.2.24"),
        ];
        let out = match_updates(&catalog, &after);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].soft_id, "kimi");
        assert_eq!(out[0].current, "3.2.24");
    }

    #[test]
    fn an_empty_catalog_never_produces_rows() {
        // Without a catalog the local match has nothing to compare against;
        // the caller drops the cached list rather than keeping it as truth.
        let local = vec![installed("微信", "4.2.0.53")];
        assert!(rescan_with(&[], &local, &[]).is_empty());
    }

    #[test]
    fn stale_duplicate_keys_do_not_report_an_update() {
        // The machine has Git 2.56.1 plus a leftover "GitHub 1.0" key that
        // used to match "Git" and kept it in the list forever.
        let catalog = vec![app("Git", "2.56.1", "git")];
        let local = vec![installed("Git", "2.56.1"), installed("GitHub", "1.0")];
        assert!(match_updates(&catalog, &local).is_empty());
    }

    #[test]
    fn one_local_app_uses_the_newest_catalog_row() {
        // Tencent still lists 微信 3.9 while Lenovo lists 微信电脑版 4.2.1.9;
        // an installed 4.2.0.13 must be compared against the newest row only.
        let catalog = vec![
            app("微信", "3.9", "tencent"),
            app("微信电脑版", "4.2.1.9", "lenovo"),
        ];
        let local = vec![installed("微信", "4.2.0.13")];
        let out = match_updates(&catalog, &local);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].soft_id, "lenovo");
        assert_eq!(out[0].latest, "4.2.1.9");
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
                hash: None,
            },
            UpdateItem {
                soft_id: "2".into(),
                name: "QQ".into(),
                current: "9.9.30".into(),
                latest: "9.9.37".into(),
                size: None,
                logo: None,
                url: String::new(),
                hash: None,
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

    /// Diagnostic against the real registry and the cached catalog.
    #[test]
    #[ignore = "reads the local registry and the cached catalog"]
    fn diag_local_cache() {
        let cache = load_cache();
        let local = installed::scan();
        let items = match_updates(&cache.catalog, &local);
        println!(
            "{} updates from {} catalog rows, {} local keys",
            items.len(),
            cache.catalog.len(),
            local.len()
        );
        for i in &items {
            println!("  {} : {} -> {}", i.name, i.current, i.latest);
        }
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
