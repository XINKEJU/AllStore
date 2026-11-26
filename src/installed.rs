//! Local inventory of installed desktop applications, read from the Uninstall
//! registry keys. This data never leaves the machine: matching against the
//! store catalog happens locally (see `update.rs`).

use std::collections::HashMap;
use std::path::PathBuf;

use windows::Win32::System::Registry::*;
use windows::core::HSTRING;

#[derive(Debug, Clone, Default)]
pub struct Installed {
    pub name: String,
    pub version: String,
    /// `DisplayIcon` — usually the main executable, sometimes with `,0`.
    pub icon: Option<String>,
    /// `InstallLocation` — used as a fallback when the icon is not an exe.
    pub location: Option<String>,
    /// `Publisher` — shown on the "我的软件" page.
    pub publisher: Option<String>,
    /// `UninstallString` — the vendor's own remover, run only after the user
    /// confirms in a modal prompt. `QuietUninstallString` is preferred when
    /// present because it skips the vendor's ad-laden wizard.
    pub uninstall: Option<String>,
}

const HKLM_PATHS: [&str; 2] = [
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
    r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
];
const HKCU_PATH: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";

fn read_bytes(key: HKEY, name: &str) -> Option<Vec<u8>> {
    unsafe {
        let n = HSTRING::from(name);
        let mut size = 0u32;
        if RegQueryValueExW(key, &n, None, None, None, Some(&mut size)).is_err() {
            return None;
        }
        if size == 0 || size > 64 * 1024 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let mut kind = REG_VALUE_TYPE(0);
        if RegQueryValueExW(
            key,
            &n,
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        )
        .is_err()
        {
            return None;
        }
        buf.truncate((size as usize).min(buf.len()));
        Some(buf)
    }
}

fn wide_to_string(bytes: &[u8]) -> String {
    let units = bytes.len() / 2;
    let wide: Vec<u16> = (0..units)
        .map(|i| u16::from_ne_bytes([bytes[i * 2], bytes[i * 2 + 1]]))
        .collect();
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end]).trim().to_string()
}

fn read_string(key: HKEY, name: &str) -> Option<String> {
    let s = wide_to_string(&read_bytes(key, name)?);
    if s.is_empty() { None } else { Some(s) }
}

fn read_dword(key: HKEY, name: &str) -> u32 {
    match read_bytes(key, name) {
        Some(b) if b.len() >= 4 => u32::from_ne_bytes([b[0], b[1], b[2], b[3]]),
        _ => 0,
    }
}

fn is_system_patch(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.contains("update for ")
        || lower.contains("security update")
        || lower.contains("hotfix for")
        || lower.contains("(kb")
    {
        return true;
    }
    if let Some(rest) = lower.strip_prefix("kb")
        && rest.chars().take_while(|c| c.is_ascii_digit()).count() >= 6
    {
        return true;
    }
    false
}

/// Runtime/driver entries are not store applications; keeping them out makes
/// the update list meaningful.
fn is_runtime(name: &str, publisher: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n.contains("redistributable") || n.contains("runtime pack") || n.contains("windows sdk") {
        return true;
    }
    publisher.to_ascii_lowercase().contains("microsoft")
        && (n.contains("visual c++") || n.contains(".net") || n.contains("asp.net"))
}

fn scan_root(root: HKEY, path: &str, out: &mut Vec<Installed>) {
    unsafe {
        let sub = HSTRING::from(path);
        let mut h = HKEY::default();
        if RegOpenKeyExW(root, &sub, None, KEY_READ, &mut h).is_err() {
            return;
        }
        let mut index = 0u32;
        loop {
            let mut name_buf = [0u16; 256];
            if RegEnumKeyW(h, index, Some(&mut name_buf[..])).is_err() {
                break;
            }
            index += 1;
            if index > 4096 {
                break;
            }
            let nlen = name_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(name_buf.len());
            let child = String::from_utf16_lossy(&name_buf[..nlen]);
            if child.is_empty() {
                continue;
            }
            let full = HSTRING::from(format!("{}\\{}", path, child));
            let mut item = HKEY::default();
            if RegOpenKeyExW(root, &full, None, KEY_READ, &mut item).is_err() {
                continue;
            }
            let name = read_string(item, "DisplayName");
            // A missing DisplayVersion is common for Chinese desktop apps.
            // Keep those entries for the "installed" badge; version comparison
            // simply declines to flag them as outdated.
            let version = read_string(item, "DisplayVersion").unwrap_or_default();
            let icon = read_string(item, "DisplayIcon");
            let location = read_string(item, "InstallLocation");
            let publisher = read_string(item, "Publisher").unwrap_or_default();
            let uninstall = read_string(item, "QuietUninstallString")
                .or_else(|| read_string(item, "UninstallString"));
            let system = read_dword(item, "SystemComponent");
            let parent = read_string(item, "ParentKeyName");
            let release = read_string(item, "ReleaseType");
            let _ = RegCloseKey(item);

            let Some(name) = name else { continue };
            if system != 0 || parent.is_some() || release.is_some() {
                continue;
            }
            if is_system_patch(&name) || is_runtime(&name, &publisher) {
                continue;
            }
            out.push(Installed {
                name,
                version,
                icon,
                location,
                publisher: (!publisher.is_empty()).then_some(publisher),
                uninstall,
            });
        }
        let _ = RegCloseKey(h);
    }
}

/// Enumerate installed apps from the machine-wide (64/32-bit views) and the
/// per-user Uninstall keys, deduplicated by normalized name.
pub fn scan() -> Vec<Installed> {
    let mut raw = Vec::new();
    for p in HKLM_PATHS {
        scan_root(HKEY_LOCAL_MACHINE, p, &mut raw);
    }
    scan_root(HKEY_CURRENT_USER, HKCU_PATH, &mut raw);
    dedupe(raw)
}

/// Collapse the keys that describe the same product. HKLM, WOW6432Node and
/// HKCU often all carry one, and an in-place upgrade can leave the old key
/// behind; the live install is the one with the **newest** version. Picking
/// whichever key the enumeration reached first is how an app that was just
/// updated went on being offered an update.
fn dedupe(raw: Vec<Installed>) -> Vec<Installed> {
    let mut map: HashMap<String, Installed> = HashMap::new();
    for i in raw {
        let k = normalize(&i.name);
        if k.is_empty() {
            continue;
        }
        if map.get(&k).is_some_and(|prev| !preferred(&i, prev)) {
            continue;
        }
        map.insert(k, i);
    }
    let mut out: Vec<Installed> = map.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Is `candidate` a better description of the same product than `current`?
/// Version first (a real number beats a missing one, a newer number beats an
/// older one), then the entry that can actually be launched.
fn preferred(candidate: &Installed, current: &Installed) -> bool {
    let (cv, kv) = (candidate.version.trim(), current.version.trim());
    match (cv.is_empty(), kv.is_empty()) {
        (true, false) => return false,
        (false, true) => return true,
        (true, true) => {}
        (false, false) => {
            if is_newer(cv, kv) {
                return true;
            }
            if is_newer(kv, cv) {
                return false;
            }
        }
    }
    let rank = |x: &Installed| (u8::from(x.icon.is_some()), u8::from(x.location.is_some()));
    rank(candidate) > rank(current)
}

/// Lowercase alphanumerics with single spaces kept as word boundaries, plus
/// the marketing suffixes store catalogues love to append.
///
/// Keeping the space matters: "git" must not look like a prefix of "github",
/// while "python" still has to match "Python 3.15.0 (64-bit)" — the only way
/// to tell those apart is to know where one word ends.
pub fn normalize(s: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            for l in c.to_lowercase() {
                out.push(l);
            }
        } else if !out.is_empty() {
            gap = true;
        }
    }
    // Longest first, so "windows 版" is removed before the bare "版".
    const SUFFIXES: [&str; 25] = [
        "智慧办公软件",
        "稳定版",
        "腾讯 ai 工作台",
        "windows 版",
        "windows版",
        "电脑版",
        "桌面版",
        "官方版",
        "正式版",
        "最新版",
        "中文版",
        "免费版",
        "标准版",
        "64 位",
        "32 位",
        "64位",
        "32位",
        "pc 版",
        "pc版",
        "64 bit",
        "32 bit",
        "windows",
        "client",
        "x64",
        "x86",
    ];
    loop {
        let mut changed = false;
        for suf in SUFFIXES {
            let t = out.trim_end();
            if t.len() > suf.len() && t.ends_with(suf) {
                out.truncate(t.len() - suf.len());
                out = out.trim_end().to_string();
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // The catalogue often uses the Chinese product name while the uninstall
    // key keeps the English one; without this "谷歌浏览器 稳定版" never
    // matches "Google Chrome".
    if out.contains("谷歌浏览器") {
        out = out.replace("谷歌浏览器", "google chrome");
    }
    out
}

/// Conservative name match: exact after normalization, a word-boundary
/// suffix ("Chrome" vs "Google Chrome"), or a version-qualified prefix
/// ("Python" vs "python 3 15 0").
///
/// A plain substring rule used to stand here, and it was too eager in both
/// directions: "Git" matched the leftover `GitHub 1.0` uninstall key and got
/// reported as outdated forever, while "Python 3.15.0 (64-bit)" was rejected
/// as a match for "Python". Both bugs show up as a wrong update list.
/// Kept for tests; production callers normalize once and use
/// [`matches_normalized`] directly.
#[cfg(test)]
pub fn name_matches(shop: &str, local: &str) -> bool {
    matches_normalized(&normalize(shop), &normalize(local))
}

/// Same rule, but for names that were normalized once up front. The store
/// re-evaluates every visible card on each paint, so the inventory keeps a
/// normalized copy instead of allocating two strings per comparison.
pub fn matches_normalized(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    // "chrome" is the trailing words of "google chrome".
    if long.ends_with(short)
        && long.len() > short.len()
        && long.as_bytes()[long.len() - short.len() - 1] == b' '
    {
        return true;
    }
    // "python" qualifies "python 3 15 0": a bare word may only extend with a
    // version, never with another product name such as "hub" or "canary".
    if let Some(rest) = long.strip_prefix(short) {
        let rest = rest.trim_start();
        return !rest.is_empty() && rest.starts_with(|c: char| c.is_ascii_digit());
    }
    false
}

/// Numeric version segments. `suffix` records that parsing stopped at a
/// non-numeric build marker (e.g. `9.0.2-Release.261011003`), which must not
/// be mistaken for an older version just because the tail was truncated.
fn segments_with_suffix(v: &str) -> (Vec<u64>, bool) {
    let s = v.trim().trim_start_matches(['v', 'V']);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut suffix = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if matches!(c, '.' | '-' | '_' | ' ') {
            if !cur.is_empty() {
                out.push(cur.parse().unwrap_or(0));
                cur.clear();
            }
        } else {
            if !cur.is_empty() {
                out.push(cur.parse().unwrap_or(0));
            }
            suffix = true;
            break;
        }
    }
    if !cur.is_empty() {
        out.push(cur.parse().unwrap_or(0));
    }
    (out, suffix)
}

#[cfg(test)]
pub fn segments(v: &str) -> Vec<u64> {
    segments_with_suffix(v).0
}

/// What the store needs to know about one catalog app on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    /// Empty when the uninstall key has no `DisplayVersion`.
    pub version: String,
    pub outdated: bool,
    /// Verified path to a real `.exe`, never an arbitrary command line.
    pub launch: Option<PathBuf>,
}

/// A snapshot of the local inventory with normalized names and resolved
/// launch paths, so painting a page of cards does no registry or disk I/O.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    apps: Vec<Installed>,
    norm: Vec<String>,
    launch: Vec<Option<PathBuf>>,
}

impl Inventory {
    pub fn new(apps: Vec<Installed>) -> Self {
        let norm = apps.iter().map(|a| normalize(&a.name)).collect();
        let launch = apps.iter().map(launch_path).collect();
        Self { apps, norm, launch }
    }

    pub fn apps(&self) -> &[Installed] {
        &self.apps
    }

    /// Match one store app against the snapshot. `shop_version` is only used
    /// for the "a newer version exists" hint; an unknown local version never
    /// produces a false update badge.
    pub fn status(&self, shop_name: &str, shop_version: Option<&str>) -> Option<InstalledApp> {
        let a = normalize(shop_name);
        if a.is_empty() {
            return None;
        }
        let mut best: Option<usize> = None;
        for (idx, n) in self.norm.iter().enumerate() {
            if !matches_normalized(&a, n) {
                continue;
            }
            let cand = &self.apps[idx];
            let better = best.is_none_or(|b| {
                let prev = &self.apps[b];
                is_newer(&cand.version, &prev.version)
                    || (cand.version == prev.version
                        && self.launch[idx].is_some()
                        && self.launch[b].is_none())
            });
            if better {
                best = Some(idx);
            }
        }
        let i = best?;
        let version = self.apps[i].version.trim().to_string();
        let outdated = shop_version
            .is_some_and(|v| !v.trim().is_empty() && !version.is_empty() && is_newer(v, &version));
        Some(InstalledApp {
            version,
            outdated,
            launch: self.launch[i].clone(),
        })
    }
}

/// Expand `%NAME%` tokens (registry values of type REG_EXPAND_SZ are not
/// expanded by RegQueryValueExW).
fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = &after[..end];
        match std::env::var(key) {
            Ok(v) => out.push_str(&v),
            Err(_) => {
                out.push('%');
                out.push_str(key);
                out.push('%');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Turn a `DisplayIcon` string into a verified executable path. Handles
/// quotes, `,0` icon indices, trailing arguments and environment variables.
fn display_exe(raw: &str) -> Option<PathBuf> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let inner = if let Some(rest) = s.strip_prefix('"') {
        rest.split('"').next().unwrap_or("").trim()
    } else {
        // "C:\app.exe,0" / "C:\app.exe -silent"
        let cut = s
            .rfind(',')
            .filter(|&i| s[i + 1..].chars().all(|c| c.is_ascii_digit() || c == '-'))
            .unwrap_or(s.len());
        let s = s[..cut].trim();
        let end = s.to_ascii_lowercase().find(".exe")? + 4;
        s[..end].trim()
    };
    let p = PathBuf::from(expand_env(inner));
    if p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
        && p.is_file()
    {
        Some(p)
    } else {
        None
    }
}

/// `InstallLocation` is only trusted when it contains exactly one plausible
/// main executable — otherwise we might launch an uninstaller or a helper.
fn folder_exe(raw: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(expand_env(raw.trim().trim_matches('"')));
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut found: Option<PathBuf> = None;
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_file()
            || !p
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("exe"))
        {
            continue;
        }
        let stem = p
            .file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if stem.contains("uninst") || stem.contains("setup") || stem.contains("卸载") {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(p);
    }
    found
}

pub fn launch_path(i: &Installed) -> Option<PathBuf> {
    if let Some(p) = i.icon.as_deref().and_then(display_exe) {
        return Some(p);
    }
    i.location.as_deref().and_then(folder_exe)
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    let (a, _) = segments_with_suffix(latest);
    let (b, b_suffix) = segments_with_suffix(current);
    if a.is_empty() || b.is_empty() {
        return false;
    }
    for i in 0..a.len().max(b.len()) {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        if x > y {
            // A truncated local build string may simply be missing the tail;
            // only trust the upgrade when it shows up within the known prefix.
            if b_suffix && i >= b.len() {
                return false;
            }
            return true;
        }
        if x < y {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_numeric_segments() {
        assert!(is_newer("1.2.4", "1.2.3"));
        assert!(is_newer("2.0", "1.9.9"));
        assert!(is_newer("10.0.26300", "10.0.22000"));
        assert!(is_newer("V3.2.1", "3.2.0"));
        assert!(is_newer("1.2.3 (64-bit)", "1.2.2"));
        assert!(!is_newer("1.2.2", "1.2.3 (64-bit)"));
        assert!(!is_newer("1.2", "1.2.0"));
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(!is_newer("", "1.0"));
    }

    #[test]
    fn build_markers_are_not_new_versions() {
        assert!(!is_newer("9.0.2.261011003", "9.0.2-Release.261011003"));
        assert!(!is_newer("1.2.3 (64-bit)", "1.2.3"));
        assert!(is_newer("9.0.3.1", "9.0.2-Release.261011003"));
        assert!(is_newer("2.0", "1.9.9 (stable)"));
    }

    #[test]
    fn names_normalize_and_match() {
        assert!(name_matches("微信", "微信 Windows 版"));
        assert!(name_matches("Google Chrome", "Google Chrome"));
        assert!(name_matches("QQ音乐", "QQ音乐 官方版"));
        assert!(!name_matches("微信", "支付宝"));
        assert!(!name_matches("A", "A B C D"));
    }

    #[test]
    fn duplicate_keys_resolve_to_the_newest_install() {
        let key = |name: &str, version: &str, icon: Option<&str>| Installed {
            name: name.into(),
            version: version.into(),
            icon: icon.map(|s| s.to_string()),
            ..Default::default()
        };
        // An in-place upgrade leaves the old key behind. Whichever order the
        // enumeration produces, the list must describe the new build.
        for raw in [
            vec![key("微信", "4.2.0.53", None), key("微信", "4.2.1.9", None)],
            vec![key("微信", "4.2.1.9", None), key("微信", "4.2.0.53", None)],
        ] {
            let out = dedupe(raw);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].version, "4.2.1.9");
        }
        // A version-less key must not displace the versioned one.
        let out = dedupe(vec![
            key("微信", "", Some("C:\\a.exe")),
            key("微信", "4.2.1.9", None),
        ]);
        assert_eq!(out[0].version, "4.2.1.9");
        // Same version in two keys: keep the one with a launchable icon.
        let out = dedupe(vec![
            key("微信", "4.2.1.9", None),
            key("微信", "4.2.1.9", Some("C:\\a.exe")),
        ]);
        assert_eq!(out[0].icon.as_deref(), Some("C:\\a.exe"));
    }

    #[test]
    fn registry_scan_finds_installed_apps() {
        let apps = scan();
        println!("scanned {} installed apps", apps.len());
        for a in apps.iter().take(8) {
            println!("  {} | {}", a.name, a.version);
        }
        assert!(!apps.is_empty(), "registry scan returned nothing");
        assert!(apps.iter().all(|a| !a.name.is_empty()));
        assert!(
            apps.iter().any(|a| !segments(&a.version).is_empty()),
            "at least one entry should carry a version"
        );
    }

    #[test]
    fn display_icon_resolves_to_a_real_executable() {
        let comspec = std::env::var("COMSPEC").unwrap_or_default();
        if comspec.is_empty() {
            return;
        }
        let quoted = format!("\"{}\",0", comspec);
        let app = Installed {
            name: "Test".into(),
            icon: Some(quoted),
            ..Default::default()
        };
        assert_eq!(launch_path(&app), Some(PathBuf::from(comspec)));
        let bogus = Installed {
            name: "Test".into(),
            icon: Some("C:\\definitely\\missing\\app.exe,1".into()),
            ..Default::default()
        };
        assert_eq!(launch_path(&bogus), None);
    }

    #[test]
    fn matching_stops_at_word_boundaries() {
        // A stale uninstall key must not impersonate a different product.
        assert!(!name_matches("Git", "GitHub"));
        assert!(!name_matches("Git", "GitHub-Store"));
        assert!(!name_matches("Google Chrome", "Google Chrome Canary"));
        assert!(!name_matches("Google Chrome Canary", "Google Chrome"));
        assert!(!name_matches("微信", "微信输入法"));
        assert!(!name_matches("微信", "微信开发者工具"));
        assert!(!name_matches("微信", "微信分身大师"));
    }

    #[test]
    fn matching_accepts_version_and_marketing_variants() {
        assert!(name_matches("Git", "Git"));
        assert!(name_matches("Python", "Python 3.15.0 (64-bit)"));
        assert!(name_matches("微信", "微信电脑版"));
        assert!(name_matches("企业微信", "企业微信 - 智慧办公软件"));
        assert!(name_matches("ima", "ima - 腾讯 AI 工作台"));
        assert!(name_matches("Google Chrome", "Chrome"));
        assert!(name_matches("谷歌浏览器 稳定版（64位）", "Google Chrome"));
        assert!(name_matches(
            "Visual Studio Code",
            "Microsoft Visual Studio Code"
        ));
    }

    #[test]
    fn inventory_reports_installed_outdated_and_missing() {
        let inv = Inventory::new(vec![
            Installed {
                name: "微信 Windows 版".into(),
                version: "3.9.8".into(),
                ..Default::default()
            },
            Installed {
                name: "Google Chrome".into(),
                version: String::new(),
                ..Default::default()
            },
        ]);
        let wechat = inv.status("微信", Some("3.9.10")).unwrap();
        assert_eq!(wechat.version, "3.9.8");
        assert!(wechat.outdated);
        assert!(!inv.status("微信", Some("3.9.8")).unwrap().outdated);
        // No local version: installed, but never falsely marked outdated.
        let chrome = inv.status("Chrome", Some("120.0")).unwrap();
        assert!(chrome.version.is_empty());
        assert!(!chrome.outdated);
        assert!(inv.status("支付宝", Some("1.0")).is_none());
    }
}
