//! Self-update check.
//!
//! Only *checks* — nothing is replaced behind the user's back. A newer tag on
//! the public GitHub releases page produces a notice on the settings page with
//! buttons to open the release or queue the installer in the normal download
//! centre (so it gets the same size checks and resume support).

use serde::Deserialize;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use crate::net;

const API: &str = "https://api.github.com/repos/XINKEJU/AllStore/releases/latest";

#[derive(Clone, Debug)]
pub struct Release {
    /// Version without the leading `v`, e.g. `0.14.0`.
    pub version: String,
    /// First line of the release body, shown as a one-line summary.
    pub summary: String,
    /// Human-facing release page.
    pub page: String,
    /// Installer asset, when the release publishes one.
    pub asset: Option<String>,
}

#[derive(Deserialize)]
struct Api {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
}

pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `0.14.1` / `v0.14.1` / `0.14.1-beta` -> (0, 14, 1). Pre-release suffixes are
/// ignored: a beta tag must not outrank the matching final release.
fn parse_version(tag: &str) -> Option<(u32, u32, u32)> {
    let t = tag.trim().trim_start_matches(['v', 'V']);
    let mut it = t.split('.');
    let major = it.next()?.trim().parse().ok()?;
    let minor = it.next()?.trim().parse().ok()?;
    let patch = it.next()?.split(['-', '+']).next()?.trim().parse().ok()?;
    Some((major, minor, patch))
}

/// `Ok(None)` means "this build is current"; `Err` is a real network/parse
/// failure worth telling the user about.
pub fn check() -> Result<Option<Release>, String> {
    let json = net::get_json_with(
        API,
        &[
            ("Accept", "application/vnd.github+json".to_string()),
            ("X-GitHub-Api-Version", "2022-11-28".to_string()),
        ],
    )?;
    let api: Api = serde_json::from_value(json).map_err(|e| format!("更新信息解析失败：{}", e))?;
    let (Some(latest), Some(mine)) = (parse_version(&api.tag_name), parse_version(current()))
    else {
        return Ok(None);
    };
    if latest <= mine {
        return Ok(None);
    }
    // Prefer the MSI: it upgrades in place and shows up in "应用和功能".
    let asset = api
        .assets
        .iter()
        .find(|a| a.name.to_ascii_lowercase().ends_with(".msi"))
        .or_else(|| {
            api.assets
                .iter()
                .find(|a| a.name.to_ascii_lowercase().ends_with(".exe"))
        })
        .filter(|a| !a.browser_download_url.is_empty())
        .map(|a| a.browser_download_url.clone());
    Ok(Some(Release {
        version: api.tag_name.trim_start_matches(['v', 'V']).to_string(),
        summary: api
            .body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("")
            .trim_start_matches(['-', '*', ' '])
            .to_string(),
        page: api.html_url,
        asset,
    }))
}

/// Open a link in the user's default browser. No custom protocol handler, no
/// embedded web view.
pub fn open_url(url: &str) {
    let target = HSTRING::from(url);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &target,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers_not_strings() {
        assert!(parse_version("v0.14.0") > parse_version("0.13.9"));
        assert!(parse_version("0.9.0") < parse_version("0.10.0"));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("nightly"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn current_build_is_parseable() {
        assert!(parse_version(current()).is_some());
    }
}
