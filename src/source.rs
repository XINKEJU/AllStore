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
/// Tencent category id → the category name the sidebar already uses (Lenovo's
/// vocabulary). Keeping one shared label is what lets a category page mix both
/// sources with a plain string comparison and no extra request.
const TENCNET_CATEGORIES: [(u32, &str); 18] = [
    (1, "聊天社交"),
    (2, "输入法"),
    (3, "浏览器"),
    (4, "下载工具"),
    (5, "视频软件"),
    (6, "音乐软件"),
    (7, "图片图形"),
    (8, "安全防护"),
    (9, "解压刻录"),
    (10, "系统工具"),
    (11, "驱动工具"),
    (12, "办公教育"),
    (13, "编程软件"),
    (14, "理财金融"),
    (15, "剪辑工具"),
    (16, "游戏"),
    (17, "助手工具"),
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

/// HEAD-probe verdicts, so a launch costs nothing after the first check.
/// The Tencent list API keeps returning links to files its CDN already
/// deleted (~half of them in testing), and downloading those fails instantly
/// with HTTP 404. Probing once and hiding the corpses is the only fix that
/// does not require an undocumented API.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct UrlVerdict {
    ok: bool,
    /// Unix seconds of the probe that produced this verdict.
    ts: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct UrlCheck {
    #[serde(default)]
    checks: std::collections::HashMap<String, UrlVerdict>,
}

/// A verdict older than this is re-probed on the next launch.
const VERDICT_TTL: u64 = 24 * 3600;
/// Verdicts this old are dropped from the file entirely.
const VERDICT_KEEP: u64 = 7 * 24 * 3600;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn check_path() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(base).join("MindosShop").join("urlcheck.json"))
}

impl UrlCheck {
    pub fn load() -> Self {
        let Some(p) = check_path() else {
            return Self::default();
        };
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// `Some(true/false)` when the verdict is still trusted, `None` when the
    /// link has never been probed or the verdict went stale.
    pub fn verdict(&self, url: &str) -> Option<bool> {
        let v = self.checks.get(url)?;
        (now_secs().saturating_sub(v.ts) < VERDICT_TTL).then_some(v.ok)
    }

    pub fn record(&mut self, url: &str, ok: bool) {
        self.checks
            .insert(url.to_string(), UrlVerdict { ok, ts: now_secs() });
    }

    pub fn save(&self) {
        let Some(p) = check_path() else { return };
        let now = now_secs();
        let mut pruned = Self::default();
        for (url, v) in &self.checks {
            if now.saturating_sub(v.ts) < VERDICT_KEEP {
                pruned.checks.insert(url.clone(), *v);
            }
        }
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string(&pruned) {
            let _ = std::fs::write(p, s);
        }
    }
}

/// Probe every URL with a bounded number of workers. Probing sequentially
/// would take a minute for a full catalog; this finishes in a few seconds.
pub fn probe_urls(urls: Vec<String>) -> Vec<(String, bool)> {
    const WORKERS: usize = 12;
    if urls.is_empty() {
        return Vec::new();
    }
    let queue = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
        urls,
    )));
    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    std::thread::scope(|scope| {
        for _ in 0..WORKERS.min(queue.lock().map(|q| q.len()).unwrap_or(0)) {
            let queue = std::sync::Arc::clone(&queue);
            let out = std::sync::Arc::clone(&out);
            scope.spawn(move || {
                loop {
                    let next = queue.lock().ok().and_then(|mut q| q.pop_front());
                    let Some(url) = next else { break };
                    let ok = net::url_alive(&url);
                    if let Ok(mut o) = out.lock() {
                        o.push((url, ok));
                    }
                }
            });
        }
    });
    std::sync::Arc::try_unwrap(out)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_default()
}

/// A replacement installer for a row whose original link rotted, learned once
/// and kept on disk so later launches skip both the probe and the search.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Mirror {
    url: String,
    ts: u64,
}

/// `name -> working installer`, written by [`repair_links`].
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Mirrors {
    #[serde(default)]
    map: std::collections::HashMap<String, Mirror>,
}

/// A mirror found two weeks ago may itself be gone; re-resolve then.
const MIRROR_TTL: u64 = 14 * 24 * 3600;

fn mirror_path() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(base).join("MindosShop").join("mirrors.json"))
}

impl Mirrors {
    pub fn load() -> Self {
        let Some(p) = mirror_path() else {
            return Self::default();
        };
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        let m = self.map.get(name)?;
        (now_secs().saturating_sub(m.ts) < MIRROR_TTL).then_some(m.url.as_str())
    }

    pub fn put(&mut self, name: &str, url: &str) {
        self.map.insert(
            name.to_string(),
            Mirror {
                url: url.to_string(),
                ts: now_secs(),
            },
        );
    }

    pub fn save(&self) {
        let Some(p) = mirror_path() else { return };
        let now = now_secs();
        let mut pruned = Mirrors::default();
        for (name, m) in &self.map {
            if now.saturating_sub(m.ts) < MIRROR_TTL {
                pruned.map.insert(name.clone(), m.clone());
            }
        }
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string(&pruned) {
            let _ = std::fs::write(p, s);
        }
    }
}

/// What [`repair_links`] did, for the log line and the status bar.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RepairStats {
    /// Links that had no verdict yet and were HEAD-probed this run.
    pub probed: usize,
    /// Links that answered 4xx/5xx or refused to resolve.
    pub dead: usize,
    /// Dead links replaced by another source's copy of the same software.
    pub rehomed: usize,
    /// Dead links nobody else carried, so the row was removed.
    pub dropped: usize,
}

/// Turn "the catalog lies about download links" into "every row downloads".
///
/// Tencent's list API still advertises installers that their CDN deleted years
/// ago — roughly a third of its rows. Their own web page is equally broken for
/// those, so there is no fresher endpoint to ask: the only reliable answer is
/// to look the software up on another source and keep that link instead.
///
/// Probes and lookups are concurrent and both answers are cached
/// (`urlcheck.json` / `mirrors.json`), so only the first launch pays the cost.
pub fn repair_links(token: &str, apps: &mut Vec<App>, note: &mut dyn FnMut(&str)) -> RepairStats {
    let mut check = UrlCheck::load();
    let mut mirrors = Mirrors::load();
    let mut stats = RepairStats::default();

    // Lenovo links are minted per request, so probing them is pointless.
    let mut unknown: Vec<String> = Vec::new();
    for a in apps.iter() {
        if a.origin() == LENOVO {
            continue;
        }
        let url = a.download_url();
        if check.verdict(&url).is_none() {
            unknown.push(url);
        }
    }
    if !unknown.is_empty() {
        note(&format!("正在校验 {} 条下载链接…", unknown.len()));
        stats.probed = unknown.len();
        for (url, ok) in probe_urls(unknown) {
            check.record(&url, ok);
        }
    }

    // Apply what we already know, then look up whatever is still unexplained.
    // Only third-party rows need a lookup; Lenovo repairs itself from its id.
    let mut replacements: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut unresolved: Vec<String> = Vec::new();
    for a in apps.iter() {
        if a.origin() == LENOVO || check.verdict(&a.download_url()) != Some(false) {
            continue;
        }
        match mirrors.get(&a.name) {
            Some(url) => {
                replacements.insert(a.name.clone(), url.to_string());
            }
            None => unresolved.push(a.name.clone()),
        }
    }
    unresolved.sort();
    unresolved.dedup();

    if !unresolved.is_empty() {
        note(&format!(
            "正在为 {} 个失效链接寻找替代安装包…",
            unresolved.len()
        ));
        const WORKERS: usize = 8;
        let found = std::sync::Mutex::new(std::collections::HashMap::<String, String>::new());
        let found_r = &found;
        std::thread::scope(|scope| {
            for chunk in unresolved.chunks(WORKERS) {
                scope.spawn(move || {
                    for name in chunk {
                        // Prefer a result from a source that hosts its own
                        // packages; another Tencent row would likely be just as
                        // stale.
                        let hit = net::search(token, name).ok().and_then(|list| {
                            let mut fallback = None;
                            for x in list {
                                let url = x.download_url();
                                if url.is_empty() {
                                    continue;
                                }
                                if x.origin() != TENCENT {
                                    return Some(url);
                                }
                                fallback.get_or_insert(url);
                            }
                            fallback
                        });
                        if let Some(url) = hit
                            && let Ok(mut f) = found_r.lock()
                        {
                            f.insert(name.clone(), url);
                        }
                    }
                });
            }
        });
        for (name, url) in found.into_inner().unwrap_or_default() {
            mirrors.put(&name, &url);
            replacements.insert(name, url);
        }
    }

    // Swap live links in, drop the rows that have no usable installer left.
    let (kept, rehomed, dropped) = apply_repairs(
        std::mem::take(apps),
        &|a| check.verdict(&a.download_url()) == Some(false),
        &replacements,
    );
    *apps = kept;
    stats.rehomed = rehomed;
    stats.dropped = dropped;
    stats.dead = rehomed + dropped;

    check.save();
    mirrors.save();
    if stats.dead > 0 {
        net::log(&format!(
            "REPAIR probed={} dead={} rehomed={} dropped={}",
            stats.probed, stats.dead, stats.rehomed, stats.dropped
        ));
    }
    stats
}

/// The mechanical half of [`repair_links`], split out so the swap/drop rules
/// can be tested without a network: every row the caller marks dead either gets
/// a replacement link (keeping its name/icon/category) or disappears.
///
/// Lenovo rows never need a lookup: when their vendor-supplied `dl_a` rots, the
/// store can mint its own `dlservice?softid=` link from the id it already has.
fn apply_repairs(
    apps: Vec<App>,
    is_dead: &dyn Fn(&App) -> bool,
    replacements: &std::collections::HashMap<String, String>,
) -> (Vec<App>, usize, usize) {
    let mut kept = Vec::with_capacity(apps.len());
    let mut rehomed = 0usize;
    let mut dropped = 0usize;
    for mut a in apps {
        if !is_dead(&a) {
            kept.push(a);
            continue;
        }
        if a.origin() == LENOVO {
            // Clearing both slots makes `download_url()` fall back to the
            // softid link, which the store mints on demand.
            a.dl_a = None;
            a.dl_b = None;
            rehomed += 1;
            kept.push(a);
            continue;
        }
        match replacements.get(&a.name) {
            Some(url) => {
                a.dl_a = Some(url.clone());
                a.dl_b = None;
                rehomed += 1;
                kept.push(a);
            }
            None => dropped += 1,
        }
    }
    (kept, rehomed, dropped)
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
/// counts requests so the UI can show a determinate bar during start-up, while
/// `note(text)` carries the free-form stage line ("正在校验 227 条下载链接…").
pub fn catalog(
    token: &str,
    progress: &mut dyn FnMut(usize, usize),
    note: &mut dyn FnMut(&str),
) -> Catalog {
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

    // Every row must have a link that answers before it reaches the UI: the
    // catalog's own `url` fields are not trustworthy on their own.
    let mut merged = dedup(out);
    let repair = repair_links(token, &mut merged, note);
    if repair.dropped > 0 {
        warnings.push(format!(
            "{} 个应用没有可用的下载地址，已隐藏",
            repair.dropped
        ));
    }

    // Concatenating the sources keeps Lenovo's 300+ rows in front, which makes
    // "全部源" look identical to "联想" on the first screen. Round-robin the
    // sources instead so the mixed list is visibly mixed.
    let merged = interleave(merged);
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
                category: Some(label.to_string()),
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
        category: text("category"),
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
    fn link_verdicts_are_cached_and_reused() {
        let mut c = UrlCheck::default();
        assert_eq!(c.verdict("https://e/a.exe"), None, "unprobed link");
        c.record("https://e/a.exe", true);
        assert_eq!(c.verdict("https://e/a.exe"), Some(true));
        c.record("https://e/b.exe", false);
        assert_eq!(c.verdict("https://e/b.exe"), Some(false));
    }

    /// Tencent's category names must be spelled exactly like the sidebar's, or
    /// a category page silently loses every Tencent row.
    #[test]
    fn tencent_categories_match_the_sidebar_vocabulary() {
        let want = [
            "聊天社交",
            "输入法",
            "浏览器",
            "下载工具",
            "视频软件",
            "音乐软件",
            "图片图形",
            "安全防护",
            "解压刻录",
            "系统工具",
            "驱动工具",
            "办公教育",
            "编程软件",
            "理财金融",
            "剪辑工具",
            "游戏",
            "助手工具",
            "桌面美化",
        ];
        let got: Vec<&str> = TENCNET_CATEGORIES.iter().map(|(_, l)| *l).collect();
        assert_eq!(got, want);
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

    fn row(name: &str, source: &str, url: &str) -> App {
        App {
            id: "T1".into(),
            name: name.into(),
            introduction: None,
            version: Some("1.0".into()),
            size: None,
            downloads: None,
            score: None,
            logo: None,
            dl_a: Some(url.into()),
            dl_b: Some("https://webcdn.m.qq.com/old.exe".into()),
            is_advert: None,
            source: Some(source.into()),
            desc: None,
            category: None,
        }
    }

    #[test]
    fn dead_rows_swap_in_a_mirror_and_keep_their_identity() {
        let apps = vec![
            row("腾讯QQ", TENCENT, "https://webcdn.m.qq.com/gone.exe"),
            row("微信", TENCENT, "https://webcdn.m.qq.com/ok.exe"),
            row(
                "千问",
                LENOVO,
                "https://pc-store.lenovomm.cn/dlservice?softid=L1",
            ),
        ];
        let dead_urls = ["https://webcdn.m.qq.com/gone.exe"];
        let mut repl = std::collections::HashMap::new();
        repl.insert(
            "腾讯QQ".to_string(),
            "https://pc-store.lenovomm.cn/dl?softid=L9".to_string(),
        );
        let (kept, rehomed, dropped) = apply_repairs(
            apps,
            &|a| dead_urls.contains(&a.download_url().as_str()),
            &repl,
        );
        assert_eq!((rehomed, dropped), (1, 0));
        assert_eq!(kept.len(), 3);
        let qq = kept.iter().find(|a| a.name == "腾讯QQ").unwrap();
        // The row keeps its Tencent identity but now points at a live file...
        assert_eq!(qq.origin(), TENCENT);
        assert_eq!(
            qq.download_url(),
            "https://pc-store.lenovomm.cn/dl?softid=L9"
        );
        // ...and the stale secondary link cannot come back to life.
        assert!(qq.dl_b.is_none());
    }

    #[test]
    fn a_dead_lenovo_vendor_link_falls_back_to_the_softid_link() {
        // Lenovo's rows sometimes carry the vendor's own CDN link, which rots
        // exactly like Tencent's. No lookup is needed: the id is enough.
        let mut a = row("千问", LENOVO, "https://vendor.example.com/gone.exe");
        a.id = "L123".into();
        let (kept, rehomed, dropped) =
            apply_repairs(vec![a], &|_| true, &std::collections::HashMap::new());
        assert_eq!((rehomed, dropped), (1, 0));
        assert_eq!(
            kept[0].download_url(),
            format!("{}/dlservice/download?softid=L123", crate::model::BASE)
        );
    }

    #[test]
    fn dead_rows_without_a_mirror_are_dropped() {
        let apps = vec![
            row("无人收录", TENCENT, "https://webcdn.m.qq.com/gone.exe"),
            row("微信", TENCENT, "https://webcdn.m.qq.com/ok.exe"),
        ];
        let (kept, rehomed, dropped) = apply_repairs(
            apps,
            &|a| a.download_url().contains("gone"),
            &std::collections::HashMap::new(),
        );
        assert_eq!((rehomed, dropped), (0, 1));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "微信");
    }

    /// Live measurement: of the Tencent links that answer 4xx, how many can be
    /// replaced by the same software on another source? Run with
    /// `cargo test -- --ignored live_rehome_coverage --nocapture`.
    #[test]
    #[ignore = "requires live network"]
    fn live_rehome_coverage() {
        use std::time::Instant;

        let token = net::register().expect("token");
        let list = tencent().expect("tencent feed");
        let all: Vec<App> = list;
        let urls: Vec<String> = all.iter().map(|a| a.download_url()).collect();
        let t0 = Instant::now();
        let verdicts = probe_urls(urls);
        let probe_secs = t0.elapsed().as_secs_f32();

        let dead: Vec<&App> = all
            .iter()
            .filter(|a| {
                verdicts
                    .iter()
                    .find(|(u, _)| *u == a.download_url())
                    .is_some_and(|(_, ok)| !ok)
            })
            .collect();
        println!(
            "tencent={} dead={} probe={:.1}s",
            all.len(),
            dead.len(),
            probe_secs
        );

        // Eight workers, same shape as the startup repair.
        let t1 = Instant::now();
        let hits = std::sync::atomic::AtomicUsize::new(0);
        let missing = std::sync::Mutex::new(Vec::<String>::new());
        let (hits_r, missing_r, token_r) = (&hits, &missing, &token);
        std::thread::scope(|s| {
            for chunk in dead.chunks(8) {
                s.spawn(move || {
                    for a in chunk {
                        let hit = net::search(token_r, &a.name)
                            .ok()
                            .and_then(|r| r.into_iter().find(|x| !x.download_url().is_empty()));
                        match hit {
                            Some(x) => {
                                hits_r.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                println!("REHOME {} -> {} ({})", a.name, x.name, x.origin());
                            }
                            None => missing_r.lock().unwrap().push(a.name.clone()),
                        }
                    }
                });
            }
        });
        let hits = hits.load(std::sync::atomic::Ordering::Relaxed);
        println!(
            "re-homed {}/{} in {:.1}s; no replacement: {:?}",
            hits,
            dead.len(),
            t1.elapsed().as_secs_f32(),
            missing.lock().unwrap()
        );
        assert!(
            hits > dead.len() / 2,
            "re-homing must rescue most dead rows"
        );
    }

    /// Live check that the merged catalog is strictly larger than Lenovo alone.
    #[test]
    #[ignore = "requires live network"]
    fn live_catalog_merges_sources() {
        let token = net::register().expect("token");
        let mut progress = |_, _| {};
        let mut note = |_: &str| {};
        let c = catalog(&token, &mut progress, &mut note);
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
