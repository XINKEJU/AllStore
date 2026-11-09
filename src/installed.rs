//! Local inventory of installed desktop applications, read from the Uninstall
//! registry keys. This data never leaves the machine: matching against the
//! store catalog happens locally (see `update.rs`).

use std::collections::HashMap;

use windows::Win32::System::Registry::*;
use windows::core::HSTRING;

#[derive(Debug, Clone)]
pub struct Installed {
    pub name: String,
    pub version: String,
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
            let version = read_string(item, "DisplayVersion");
            let publisher = read_string(item, "Publisher").unwrap_or_default();
            let system = read_dword(item, "SystemComponent");
            let parent = read_string(item, "ParentKeyName");
            let release = read_string(item, "ReleaseType");
            let _ = RegCloseKey(item);

            let (Some(name), Some(version)) = (name, version) else {
                continue;
            };
            if system != 0 || parent.is_some() || release.is_some() {
                continue;
            }
            if is_system_patch(&name) || segments(&version).is_empty() {
                continue;
            }
            if is_runtime(&name, &publisher) {
                continue;
            }
            out.push(Installed { name, version });
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

    let mut map: HashMap<String, Installed> = HashMap::new();
    for i in raw {
        let k = normalize(&i.name);
        if k.is_empty() {
            continue;
        }
        if let Some(prev) = map.get(&k)
            && !is_newer(&i.version, &prev.version)
        {
            continue;
        }
        map.insert(k, i);
    }
    let mut out: Vec<Installed> = map.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Lowercase alphanumerics only, with common marketing suffixes stripped.
pub fn normalize(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                out.push(l);
            }
        }
    }
    const SUFFIXES: [&str; 12] = [
        "windows版",
        "pc版",
        "桌面版",
        "官方版",
        "正式版",
        "最新版",
        "中文版",
        "64位",
        "32位",
        "x64",
        "x86",
        "client",
    ];
    loop {
        let mut changed = false;
        for suf in SUFFIXES {
            if out.len() > suf.len() && out.ends_with(suf) {
                out.truncate(out.len() - suf.len());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    out
}

/// Conservative name match: exact after normalization, or a contained name
/// that is long enough to avoid short-name false positives.
pub fn name_matches(shop: &str, local: &str) -> bool {
    let a = normalize(shop);
    let b = normalize(local);
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() {
        (a.as_str(), b.as_str())
    } else {
        (b.as_str(), a.as_str())
    };
    long.contains(short) && short.chars().count() >= 3 && short.len() * 2 >= long.len()
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

pub fn segments(v: &str) -> Vec<u64> {
    segments_with_suffix(v).0
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
    fn registry_scan_finds_installed_apps() {
        let apps = scan();
        println!("scanned {} installed apps", apps.len());
        for a in apps.iter().take(8) {
            println!("  {} | {}", a.name, a.version);
        }
        assert!(!apps.is_empty(), "registry scan returned nothing");
        assert!(
            apps.iter()
                .all(|a| !a.name.is_empty() && !segments(&a.version).is_empty())
        );
    }
}
