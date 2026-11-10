//! Catalog aggregation across several independent software sources.
//!
//! Every feed is normalised into [`model::App`] so the cards, the update
//! matcher and the downloader never need to know where an entry came from.
//! Three kinds are supported:
//!
//! * **Lenovo** – the original token API (`net::class_apps`). Richest data
//!   (description, rating, install count) and the only source with details.
//! * **Tencent** – the public `softlistcgi.php` JSON used by 腾讯软件中心.
//!   No token, no ads, direct CDN links, verified to answer `Range` requests.
//! * **Custom** – any URL that returns a JSON array of apps; users add these
//!   in `%LOCALAPPDATA%\MindosShop\sources.json`.
//!
//! A failing source degrades to a warning instead of an empty store: the
//! catalog is the union of whatever answered.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::App;
use crate::net;

pub const LENOVO: &str = "联想";
pub const TENCENT: &str = "腾讯";

/// Tencent category ids shown on pc.qq.com. One request each; the API always
/// returns the most popular 28 entries of a category regardless of `p`/`n`.
const TENCNET_CATEGORIES: [(u32, &str); 18] = [
    (1, "聊天社交"),
    (2, "输入法"),
    (3, "浏览器"),
    (4, "网盘下载"),
    (5, "影音播放"),
    (6, "音乐"),
    (7, "图形图像"),
    (8, "安全"),
    (9, "压缩刻录"),
    (10, "系统工具"),
    (11, "驱动程序"),
    (12, "办公商务"),
    (13, "开发工具"),
    (14, "网银支付"),
    (15, "视频剪辑"),
    (16, "游戏"),
    (17, "游戏平台"),
    (18, "桌面美化"),
];

const TENCENT_LIST: &str = "https://s.pcmgr.qq.com/tapi/web/softlistcgi.php";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomSource {
    pub name: String,
    pub url: String,
}

/// User-editable source configuration. Absent file = both built-in sources on.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Display names to switch off (`联想`, `腾讯`, or a custom source name).
    #[serde(default)]
    pub disabled: Vec<String>,
    #[serde(default)]
    pub custom: Vec<CustomSource>,
}

fn settings_path() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(base).join("MindosShop").join("sources.json"))
}

pub fn load_settings() -> Settings {
    let Some(p) = settings_path() else {
        return Settings::default();
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
        .unwrap_or_default()
}

pub struct Catalog {
    pub apps: Vec<App>,
    /// Source names that actually contributed at least one app.
    pub sources: Vec<String>,
    /// Human-readable per-source failures, surfaced in the status bar/log.
    pub warnings: Vec<String>,
}

/// Fetch every enabled source and merge the results. `progress(done, total)`
/// counts requests so the UI can show a determinate bar during start-up.
pub fn catalog(token: &str, progress: &mut dyn FnMut(usize, usize)) -> Catalog {
    let settings = load_settings();
    let on = |name: &str| !settings.disabled.iter().any(|d| d == name);
    let mut jobs: Vec<String> = Vec::new();
    if on(LENOVO) {
        jobs.push(LENOVO.into());
    }
    if on(TENCENT) {
        jobs.push(TENCENT.into());
    }
    for c in &settings.custom {
        if on(&c.name) {
            jobs.push(c.name.clone());
        }
    }
    let total = jobs.len().max(1);
    progress(0, total);

    let mut out: Vec<App> = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut done = 0usize;

    for name in &jobs {
        let result = if name == LENOVO {
            lenovo(token)
        } else if name == TENCENT {
            tencent()
        } else {
            match settings.custom.iter().find(|c| &c.name == name) {
                Some(c) => custom(c),
                None => Err("配置已变更".into()),
            }
        };
        match result {
            Ok(list) if !list.is_empty() => {
                net::log(&format!("SOURCE {} apps={}", name, list.len()));
                sources.push(name.clone());
                out.extend(list);
            }
            Ok(_) => warnings.push(format!("{} 源没有返回任何应用", name)),
            Err(e) => {
                net::log(&format!("SOURCE {} FAIL {}", name, e));
                warnings.push(format!("{} 源不可用：{}", name, e));
            }
        }
        done += 1;
        progress(done, total);
    }

    // Concatenating the sources keeps Lenovo's 300+ rows in front, which makes
    // "全部源" look identical to "联想" on the first screen. Round-robin the
    // sources instead so the mixed list is visibly mixed.
    let merged = interleave(dedup(out));
    Catalog {
        apps: merged,
        sources,
        warnings,
    }
}

/// Round-robin one entry per source at a time, preserving each source's own
/// order. Sources are taken in first-seen order, so the flagship rows of every
/// source land on the first screen of the merged list.
pub fn interleave(list: Vec<App>) -> Vec<App> {
    let mut order: Vec<String> = Vec::new();
    for a in &list {
        let src = a.origin().to_string();
        if !order.contains(&src) {
            order.push(src);
        }
    }
    if order.len() < 2 {
        return list;
    }
    let mut buckets: Vec<std::collections::VecDeque<App>> = order
        .iter()
        .map(|_| std::collections::VecDeque::new())
        .collect();
    for a in list {
        let idx = order
            .iter()
            .position(|s| s == a.origin())
            .unwrap_or_default();
        buckets[idx].push_back(a);
    }
    let mut out = Vec::new();
    while buckets.iter().any(|b| !b.is_empty()) {
        for b in buckets.iter_mut() {
            if let Some(a) = b.pop_front() {
                out.push(a);
            }
        }
    }
    out
}

/// One product can appear in several categories (and in several sources).
/// Keep the first occurrence: the order is Lenovo → Tencent → custom, i.e.
/// richest metadata first.
pub fn dedup(list: Vec<App>) -> Vec<App> {
    let mut out: Vec<App> = Vec::with_capacity(list.len());
    for a in list {
        if a.name.trim().is_empty() {
            continue;
        }
        // Lenovo rows can omit both URL fields and still be downloadable via
        // the dlservice fallback; every other source must ship a real link.
        let linked = a.id.starts_with('L')
            || [&a.dl_a, &a.dl_b]
                .into_iter()
                .flatten()
                .any(|u| u.starts_with("http"));
        if !linked {
            continue;
        }
        let key = normalise(&a.name);
        if out.iter().any(|x| normalise(&x.name) == key) {
            continue;
        }
        out.push(a);
    }
    out
}

fn normalise(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Case-insensitive substring match used for the local part of a search, so
/// Tencent and custom entries are findable without a per-source search API.
pub fn search_local(list: &[App], keyword: &str) -> Vec<App> {
    let kw = keyword.trim().to_lowercase();
    if kw.is_empty() {
        return Vec::new();
    }
    list.iter()
        .filter(|a| a.name.to_lowercase().contains(&kw))
        .cloned()
        .collect()
}

fn lenovo(token: &str) -> Result<Vec<App>, String> {
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
    let mut out = Vec::new();
    let mut failures = 0usize;
    for (code, id) in &targets {
        match net::class_apps(token, code, id, 60) {
            Ok(list) => out.extend(list),
            Err(_) => failures += 1,
        }
    }
    if out.is_empty() && failures > 0 {
        return Err("目录接口无响应".into());
    }
    for a in &mut out {
        a.source = Some(LENOVO.into());
    }
    Ok(out)
}

/// Tencent's list endpoint. Fields: `s` id, `sn` name, `ver`, `fs` size,
/// `url` CDN link, `lg` protocol-relative logo, `star` 0-100.
fn tencent() -> Result<Vec<App>, String> {
    let mut out: Vec<App> = Vec::new();
    let mut failures = 0usize;
    for (cat, label) in TENCNET_CATEGORIES {
        let url = format!("{}?c={}&p=1&n=60&type=0", TENCENT_LIST, cat);
        let Ok(v) = net::fetch_json(&url) else {
            failures += 1;
            continue;
        };
        for item in v
            .get("list")
            .and_then(|l| l.as_array())
            .into_iter()
            .flatten()
        {
            let text = |k: &str| {
                item.get(k)
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            let name = text("sn");
            let url = text("url");
            if name.is_empty() || !url.starts_with("http") {
                continue;
            }
            let id = item
                .get("s")
                .map(|x| match x {
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::String(s) => s.clone(),
                    _ => String::new(),
                })
                .unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            let logo = text("lg");
            let logo = if logo.starts_with("//") {
                Some(format!("https:{}", logo))
            } else if logo.starts_with("http") {
                Some(logo)
            } else {
                None
            };
            let star = item.get("star").and_then(|x| x.as_u64()).unwrap_or(0);
            out.push(App {
                // Prefix keeps the id unique across sources and tells the
                // detail page that no rich detail endpoint exists.
                id: format!("T{}", id),
                name,
                introduction: None,
                desc: Some(format!("腾讯软件中心 · {}", label)),
                version: Some(text("ver")).filter(|v| !v.is_empty()),
                size: Some(text("fs")).filter(|s| !s.is_empty()),
                downloads: None,
                score: (star > 0).then(|| serde_json::json!(format!("{:.1}", star as f64 / 10.0))),
                logo,
                dl_a: Some(url),
                dl_b: None,
                is_advert: None,
                source: Some(TENCENT.into()),
            });
        }
    }
    if out.is_empty() {
        return Err(if failures > 0 {
            "列表接口无响应".into()
        } else {
            "列表为空".to_string()
        });
    }
    Ok(out)
}

/// A custom feed is a JSON array (or `{"apps":[...]}`) whose entries use the
/// same field names as the store catalog: name, version, url, size, logo,
/// desc. Anything else is ignored rather than failing the whole source.
fn custom(src: &CustomSource) -> Result<Vec<App>, String> {
    let v = net::fetch_json(&src.url)?;
    let arr = v
        .as_array()
        .cloned()
        .or_else(|| v.get("apps").and_then(|a| a.as_array()).cloned())
        .ok_or_else(|| "不是数组格式".to_string())?;
    let mut out = Vec::new();
    for item in arr {
        let Some(a) = parse_custom(&item, &src.name) else {
            continue;
        };
        out.push(a);
    }
    if out.is_empty() {
        return Err("没有可用条目".into());
    }
    Ok(out)
}

fn parse_custom(item: &serde_json::Value, source: &str) -> Option<App> {
    let text = |k: &str| {
        item.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
    };
    let name = text("name").filter(|s| !s.is_empty())?;
    let url = text("url")
        .or_else(|| text("download"))
        .filter(|u| u.starts_with("http"))?;
    let id = format!(
        "C{}",
        item.get("id")
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{:x}", hash(&format!("{}|{}", source, name))))
    );
    Some(App {
        id,
        name,
        introduction: None,
        desc: text("desc").or_else(|| text("description")),
        version: text("version"),
        size: text("size"),
        downloads: text("downloads"),
        score: item.get("score").cloned(),
        logo: text("logo").filter(|l| l.starts_with("http")),
        dl_a: Some(url),
        dl_b: None,
        is_advert: None,
        source: Some(source.to_string()),
    })
}

fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, url: &str) -> App {
        App {
            id: name.into(),
            name: name.into(),
            dl_a: Some(url.into()),
            ..Default::default()
        }
    }

    fn sourced(name: &str, source: &str) -> App {
        App {
            id: format!("{}{}", source, name),
            name: name.into(),
            dl_a: Some(format!("https://e/{}.exe", name)),
            source: Some(source.into()),
            ..Default::default()
        }
    }

    #[test]
    fn interleave_mixes_sources_without_dropping_any() {
        let list = vec![
            sourced("联想A", "联想"),
            sourced("联想B", "联想"),
            sourced("联想C", "联想"),
            sourced("腾讯A", "腾讯"),
            sourced("腾讯B", "腾讯"),
        ];
        let out = interleave(list);
        let names: Vec<&str> = out.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            ["联想A", "腾讯A", "联想B", "腾讯B", "联想C"],
            "one entry per source per round, order inside a source preserved"
        );
    }

    #[test]
    fn single_source_list_is_left_untouched() {
        let list = vec![sourced("A", "联想"), sourced("B", "联想")];
        let out = interleave(list);
        assert_eq!(out[0].name, "A");
        assert_eq!(out[1].name, "B");
    }

    #[test]
    fn dedup_keeps_the_richest_first_entry() {
        let list = vec![
            app("微信", "https://a/1.exe"),
            app("微 信", "https://b/2.exe"),
            app("WeChat", "https://c/3.exe"),
            app("坏条目", ""),
        ];
        let out = dedup(list);
        assert_eq!(out.len(), 2, "normalised names must collapse duplicates");
        assert_eq!(out[0].dl_a.as_deref(), Some("https://a/1.exe"));
        assert_eq!(out[1].name, "WeChat");
    }

    #[test]
    fn custom_entries_map_onto_the_app_model() {
        let v = serde_json::json!({
            "name": "私有工具",
            "version": "1.2.3",
            "url": "https://example.com/tool.exe",
            "size": "12 MB",
            "logo": "https://example.com/i.png",
            "desc": "内部分发"
        });
        let a = parse_custom(&v, "内网").expect("valid entry");
        assert!(a.id.starts_with('C'));
        assert_eq!(a.source.as_deref(), Some("内网"));
        assert_eq!(a.desc.as_deref(), Some("内部分发"));
        assert_eq!(a.version.as_deref(), Some("1.2.3"));
        // Missing url / name are dropped instead of producing a dead card.
        assert!(parse_custom(&serde_json::json!({"name": "x"}), "内网").is_none());
        assert!(parse_custom(&serde_json::json!({"url": "https://e/x.exe"}), "内网").is_none());
    }

    #[test]
    fn local_search_is_case_insensitive_and_partial() {
        let list = vec![
            app("谷歌浏览器 稳定版", "https://a/1.exe"),
            app("Chrome Beta", "https://a/2.exe"),
            app("微信", "https://a/3.exe"),
        ];
        assert_eq!(search_local(&list, "chrome").len(), 1);
        assert_eq!(search_local(&list, "浏览").len(), 1);
        assert_eq!(search_local(&list, "   ").len(), 0);
    }

    #[test]
    fn settings_file_is_optional() {
        let s = Settings::default();
        assert!(s.disabled.is_empty() && s.custom.is_empty());
        let parsed: Settings = serde_json::from_str(
            r#"{"disabled":["腾讯"],"custom":[{"name":"我的内网源","url":"https://e/a.json"}]}"#,
        )
        .unwrap();
        assert_eq!(parsed.custom[0].name, "我的内网源");
        assert_eq!(parsed.disabled, vec!["腾讯".to_string()]);
    }

    /// Live check: the Tencent feed must answer with usable rows. Run with
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires live network"]
    fn live_tencent_returns_direct_links() {
        let list = tencent().expect("tencent feed must answer");
        println!("tencent apps={}", list.len());
        assert!(list.len() > 100, "feed returned too few rows");
        assert!(list.iter().all(|a| a.download_url().starts_with("http")));
        assert!(list.iter().any(|a| a.logo.is_some()));
    }

    /// Live check that the merged catalog is strictly larger than Lenovo alone.
    #[test]
    #[ignore = "requires live network"]
    fn live_catalog_merges_sources() {
        let token = net::register().expect("token");
        let mut progress = |_, _| {};
        let c = catalog(&token, &mut progress);
        println!(
            "merged={} sources={:?} warnings={:?}",
            c.apps.len(),
            c.sources,
            c.warnings
        );
        assert!(
            c.sources.contains(&LENOVO.to_string()) && c.sources.contains(&TENCENT.to_string())
        );
        assert!(c.apps.len() > 400);
    }
}
