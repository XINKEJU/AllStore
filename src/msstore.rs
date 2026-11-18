//! Resolve `ms:PRODUCTID` to the installer published by the Microsoft Store.
//!
//! Only WPM rows (non-packaged Win32 software) are accepted: their manifest
//! carries a plain `https://` installer URL and its SHA-256. MSIX/APPX rows
//! resolve to a package that cannot be launched with ShellExecute, so they are
//! rejected here instead of producing a card that fails on the last click.
//!
//! No SDK, token or new crate: the Store's public edge endpoint answers to
//! the same WinHTTP client the rest of the app uses. Resolution happens at
//! download time, never while building the catalog, so a feed only has to
//! carry a short product id.

use serde_json::Value;

const BASE: &str = "https://storeedgefd.dsx.mp.microsoft.com/v9.0";
const UA: &str = "WindowsStore/22106.1401.2.0";

fn headers() -> [(&'static str, String); 3] {
    [
        ("User-Agent", UA.into()),
        ("Accept", "*/*".into()),
        ("Accept-Language", "en-US".into()),
    ]
}

/// Store product ids are 12-character uppercase base32-ish strings (`XP…` for
/// Win32, `9N…` for MSIX), but only the character shape matters: feeds may
/// contain lowercase or a differently sized id, and the endpoint decides.
pub fn valid_id(spec: &str) -> bool {
    let id = spec.strip_prefix("ms:").unwrap_or(spec);
    (8..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Return the direct installer URL and, when published, its SHA-256.
pub fn resolve(spec: &str) -> Result<(String, Option<String>), String> {
    let id = spec.strip_prefix("ms:").unwrap_or(spec).trim();
    if !valid_id(id) {
        return Err(format!("无法识别的微软商店 ID：{}", spec));
    }
    let v = crate::net::get_json_with(&format!("{BASE}/packageManifests/{id}"), &headers())?;
    pick_installer(&v)
}

/// Manifests list several locales and architectures for the same version.
/// Prefer the native architecture (x86_64 build → x64), then neutral, then
/// whatever is left; a hash travels with the chosen installer so a later
/// catalog update cannot invalidate it.
fn pick_installer(v: &Value) -> Result<(String, Option<String>), String> {
    let versions = v
        .get("Data")
        .and_then(|d| d.get("Versions"))
        .and_then(Value::as_array)
        .ok_or_else(|| "商店没有返回版本信息".to_string())?;
    let native = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86"
    };
    let mut fallback: Option<(String, Option<String>)> = None;
    for ver in versions {
        let Some(installers) = ver.get("Installers").and_then(Value::as_array) else {
            continue;
        };
        for ins in installers {
            let Some(url) = ins
                .get("InstallerUrl")
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("https://"))
            else {
                continue;
            };
            // EXE / MSI are launched by ShellExecute. APPX / MSIX are not.
            let kind = ins
                .get("InstallerType")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !matches!(kind, "exe" | "msi" | "msu") {
                continue;
            }
            let hash = ins
                .get("InstallerSha256")
                .and_then(Value::as_str)
                .and_then(crate::hash::normalize);
            let candidate = (url.to_string(), hash);
            let arch = ins
                .get("Architecture")
                .and_then(Value::as_str)
                .unwrap_or("");
            if arch == native || arch == "neutral" {
                return Ok(candidate);
            }
            if fallback.is_none() {
                fallback = Some(candidate);
            }
        }
    }
    fallback.ok_or_else(|| "该商店应用没有可直接安装的 Win32 版本（可能是 MSIX 包）".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_non_store_specs() {
        assert!(!valid_id("https://example.com/a.exe"));
        assert!(!valid_id("ms:"));
        assert!(!valid_id("ms:bad/id"));
        assert!(valid_id("ms:XPDM1ZW6815MQM"));
        assert!(valid_id("XPDM1ZW6815MQM"));
    }

    #[test]
    fn picks_native_installer_and_keeps_hash() {
        let v = json!({"Data":{"Versions":[{"Installers":[
            {"InstallerType":"exe","Architecture":"arm64","InstallerUrl":"https://cdn/arm.exe","InstallerSha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            {"InstallerType":"exe","Architecture":"x64","InstallerUrl":"https://cdn/x64.exe","InstallerSha256":"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"}
        ]}]}});
        let (url, hash) = pick_installer(&v).expect("x64 installer");
        assert_eq!(url, "https://cdn/x64.exe");
        assert_eq!(
            hash.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn skips_packaged_apps_and_http_urls() {
        let v = json!({"Data":{"Versions":[{"Installers":[
            {"InstallerType":"msix","Architecture":"x64","InstallerUrl":"https://cdn/a.msix"},
            {"InstallerType":"exe","Architecture":"x64","InstallerUrl":"http://cdn/insecure.exe"}
        ]}]}});
        assert!(pick_installer(&v).is_err(), "no usable Win32 installer");
    }

    #[test]
    fn missing_manifest_is_an_error_not_a_panic() {
        assert!(pick_installer(&json!({})).is_err());
        assert!(pick_installer(&json!({"Data":{"Versions":[]}})).is_err());
    }

    #[test]
    #[ignore = "requires live Microsoft Store API"]
    fn live_store_manifest_resolves_installer() {
        // VLC is a WPM row: its manifest publishes an EXE and a SHA-256.
        let (url, hash) = resolve("ms:XPDM1ZW6815MQM").expect("VLC must resolve");
        assert!(url.starts_with("https://"), "unexpected URL: {url}");
        assert_eq!(hash.as_deref().map(str::len), Some(64), "missing digest");
        println!("resolved {url} {hash:?}");
    }
}
