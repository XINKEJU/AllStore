//! Data model for the Lenovo content API. Only store-relevant fields are kept;
//! promotional / telemetry endpoints are never requested.
use serde::{Deserialize, Serialize};

pub const CH: &str = "9.0.3200.0917";
pub const BASE: &str = "https://pc-store.lenovomm.cn";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct App {
    #[serde(rename = "softID", default)]
    pub id: String,
    #[serde(rename = "softName", default)]
    pub name: String,
    #[serde(default)]
    pub introduction: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(rename = "installFileSize", default)]
    pub size: Option<String>,
    #[serde(rename = "downloadCount", default)]
    pub downloads: Option<String>,
    #[serde(default)]
    pub score: Option<serde_json::Value>,
    #[serde(rename = "logoFile", default)]
    pub logo: Option<String>,
    #[serde(rename = "downLoadUrl", default)]
    pub dl_a: Option<String>,
    #[serde(rename = "downloadUrl", default)]
    pub dl_b: Option<String>,
    #[serde(default)]
    #[serde(rename = "isAdvert")]
    pub is_advert: Option<bool>,
    /// Display name of the feed that produced this row (`联想`, `腾讯`, …).
    /// Set by `source` when catalogs are merged; absent in older caches.
    #[serde(default)]
    pub source: Option<String>,
    /// Short description supplied by feeds that have no HTML intro (Tencent
    /// only publishes a category, custom feeds may ship a sentence).
    #[serde(default)]
    pub desc: Option<String>,
    /// Category in the *store's* vocabulary (`聊天社交`, `浏览器`, …), so a
    /// category page can mix rows from every source without another request.
    #[serde(default)]
    pub category: Option<String>,
}

impl App {
    pub fn logo_url(&self) -> Option<&str> {
        self.logo.as_deref().filter(|s| s.starts_with("http"))
    }
    /// Feed this row came from, for the "来源" chip and the log line.
    pub fn origin(&self) -> &str {
        self.source.as_deref().unwrap_or("联想")
    }
    /// Only the Lenovo catalog has a details endpoint; other rows render from
    /// the list metadata instead of issuing a request that is certain to fail.
    pub fn has_remote_detail(&self) -> bool {
        self.id.starts_with('L')
    }
    pub fn download_url(&self) -> String {
        for u in [&self.dl_a, &self.dl_b] {
            // `gh:owner/repo` is a promise, not an address: the installer is
            // resolved when the user actually asks for it (see `source`).
            if let Some(s) = u
                && (s.starts_with("http") || s.starts_with("gh:"))
            {
                return s.clone();
            }
        }
        format!("{}/dlservice/download?softid={}", BASE, self.id)
    }
    /// True when the link is a placeholder that must be resolved before use.
    pub fn is_lazy_link(&self) -> bool {
        self.download_url().starts_with("gh:")
    }
    pub fn score_text(&self) -> String {
        match &self.score {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    }
    pub fn subtitle(&self) -> String {
        if let Some(d) = self.desc.as_deref().filter(|d| !d.trim().is_empty()) {
            return html_to_text(d);
        }
        html_to_text(self.introduction.as_deref().unwrap_or(""))
    }
    pub fn meta(&self) -> String {
        let mut v = Vec::new();
        if let Some(s) = &self.size
            && !s.trim().is_empty()
        {
            v.push(s.trim().to_string());
        }
        if let Some(s) = &self.downloads
            && !s.trim().is_empty()
        {
            v.push(format!("{} 安装", s.trim()));
        }
        // Keep the card meta to one line; the full version lives in details.
        v.join("  ·  ")
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClassItem {
    #[serde(rename = "classId", default)]
    pub id: String,
    #[serde(rename = "className", default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClassGroup {
    #[serde(default)]
    pub code: String,
    #[serde(rename = "appClassList", default)]
    pub list: Vec<ClassItem>,
}

/// The details endpoint mixes scalars and objects: `developer` is `{}` or an
/// object (never a string), `score` may be a number or a string. Typing them
/// as `Value` keeps deserialization from failing on every single app — which
/// previously made the long description silently fall back to the one-liner.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Detail {
    #[serde(rename = "detailInfo", default)]
    pub detail_info: Option<String>,
    #[serde(rename = "whatNew", default)]
    pub what_new: Option<String>,
    #[serde(default)]
    pub developer: Option<serde_json::Value>,
    #[serde(rename = "downloadUrl", default)]
    pub download_url: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(rename = "installFileSize", default)]
    pub size: Option<String>,
    #[serde(rename = "downloadCount", default)]
    pub downloads: Option<String>,
    #[serde(default)]
    pub score: Option<serde_json::Value>,
}

impl Detail {
    pub fn developer_name(&self) -> String {
        match &self.developer {
            Some(serde_json::Value::String(s)) => s.trim().to_string(),
            Some(serde_json::Value::Object(m)) => m
                .get("name")
                .or_else(|| m.get("developerName"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string(),
            _ => String::new(),
        }
    }

    pub fn score_text(&self) -> String {
        match &self.score {
            Some(serde_json::Value::String(s)) => s.trim().to_string(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    }
}

pub struct Payload {
    pub status: i32,
    pub data: serde_json::Value,
}

pub fn payload(bytes: &[u8]) -> Result<Payload, String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| format!("JSON: {}", e))?;
    // Some endpoints return `status` as a string ("0"); both forms are legal.
    let status = v
        .get("status")
        .and_then(|x| match x {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        })
        .unwrap_or(-1) as i32;
    Ok(Payload {
        status,
        data: v.get("data").cloned().unwrap_or(serde_json::Value::Null),
    })
}

/// Drop ad entries (`isAdvert`) and empty placeholders. This is the purity gate.
/// Fields are read defensively: the search endpoint mixes strings, numbers and
/// booleans for the same keys, so a strict struct deserialization would drop
/// every result.
pub fn parse_apps(data: &serde_json::Value) -> Vec<App> {
    let mut out = Vec::new();
    let arr = match data.get("apps").and_then(|x| x.as_array()) {
        Some(a) => a,
        None => return out,
    };
    for v in arr {
        let o = match v.as_object() {
            Some(o) => o,
            None => continue,
        };
        let text = |k: &str| -> Option<String> {
            match o.get(k) {
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                Some(serde_json::Value::Number(n)) => Some(n.to_string()),
                _ => None,
            }
        };
        let is_ad = match o.get("isAdvert") {
            Some(serde_json::Value::Bool(b)) => Some(*b),
            Some(serde_json::Value::Number(n)) => Some(n.as_i64().unwrap_or(0) != 0),
            Some(serde_json::Value::String(s)) => Some(s == "1" || s.eq_ignore_ascii_case("true")),
            _ => None,
        };
        let a = App {
            id: text("softID").unwrap_or_default(),
            name: text("softName").unwrap_or_default(),
            introduction: text("introduction"),
            version: text("version")
                .or_else(|| text("softVersion"))
                .or_else(|| text("versionName"))
                .or_else(|| text("appVersion")),
            size: text("installFileSize"),
            downloads: text("downloadCount"),
            score: o.get("score").cloned(),
            logo: text("logoFile"),
            dl_a: text("downLoadUrl"),
            dl_b: text("downloadUrl"),
            is_advert: is_ad,
            source: None,
            desc: None,
            category: None,
        };
        if a.is_advert == Some(true) || a.name.trim().is_empty() {
            continue;
        }
        out.push(a);
    }
    out
}

pub fn parse_classes(data: &serde_json::Value) -> Vec<ClassGroup> {
    let mut out = Vec::new();
    if let Some(arr) = data.as_array() {
        for v in arr {
            if let Ok(g) = serde_json::from_value::<ClassGroup>(v.clone()) {
                out.push(g);
            }
        }
    }
    out
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Store descriptions are HTML fragments (`<p><b><font color=...>`). Drawing
/// them verbatim shows raw markup, so strip tags, turn block tags into line
/// breaks and decode the handful of entities the feed actually uses.
pub fn html_to_text(s: &str) -> String {
    const BREAKS: [&str; 8] = ["br", "p", "div", "li", "tr", "h1", "h2", "h3"];
    let mut out = String::new();
    let mut tag = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' => {
                in_tag = false;
                let name = tag
                    .trim()
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if BREAKS.contains(&name.as_str()) {
                    out.push('\n');
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace('\r', "\n");
    let mut lines: Vec<&str> = Vec::new();
    for line in decoded.lines() {
        let t = line.trim();
        if t.is_empty() && lines.last().is_some_and(|l| l.is_empty()) {
            continue;
        }
        lines.push(t);
    }
    let text = lines.join("\n").trim().to_string();
    if text.is_empty() { String::new() } else { text }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the real API returns `developer: {}` and a numeric `score`.
    /// The old `Option<String>` field made every details request fail, so the
    /// long description never reached the detail page.
    #[test]
    fn detail_parses_real_api_shape() {
        let v = serde_json::json!({
            "softID": "L120416",
            "softName": "千问",
            "version": "5.1.5.294",
            "installFileSize": "286.62 MB",
            "downloadCount": "272万次",
            "score": 64,
            "developer": {},
            "detailInfo": "第一行\n第二行",
            "whatNew": "优化体验",
            "downloadUrl": "https://pc-store.lenovomm.cn/dlservice/download?softid=L120416",
            "captureFileList": [{"type": "1", "url": "https://example.com/a.png"}]
        });
        let d: Detail = serde_json::from_value(v).expect("details must deserialize");
        assert_eq!(d.detail_info.as_deref(), Some("第一行\n第二行"));
        assert_eq!(d.version.as_deref(), Some("5.1.5.294"));
        assert_eq!(d.score_text(), "64");
        assert!(d.developer_name().is_empty());
        assert_eq!(d.what_new.as_deref(), Some("优化体验"));
    }

    #[test]
    fn html_markup_is_stripped_from_descriptions() {
        let raw = "<p><b><font color=\"#c24f4a\">不能卸载</font></b></p><p>第二行<br/>换行&nbsp;A &amp; B</p>";
        let text = html_to_text(raw);
        assert!(
            !text.contains('<') && !text.contains('>'),
            "tags leaked: {text}"
        );
        assert!(!text.contains("font"), "attribute text leaked: {text}");
        assert!(text.contains("不能卸载"));
        assert!(text.contains("第二行"));
        assert!(text.contains("换行 A & B"), "entities not decoded: {text}");
        assert_eq!(html_to_text("  "), "");
        assert_eq!(html_to_text("纯文本"), "纯文本");
    }

    #[test]
    fn payload_accepts_numeric_and_string_status() {
        let p = payload(br#"{"status":0,"data":{"apps":[]}}"#).unwrap();
        assert_eq!(p.status, 0);
        let q = payload(br#"{"status":"0","data":null}"#).unwrap();
        assert_eq!(q.status, 0, "string status must be accepted");
    }
}
