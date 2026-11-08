//! Data model for the Lenovo content API. Only store-relevant fields are kept;
//! promotional / telemetry endpoints are never requested.
use serde::Deserialize;

pub const CH: &str = "9.0.3200.0917";
pub const BASE: &str = "https://pc-store.lenovomm.cn";

#[derive(Debug, Clone, Deserialize, Default)]
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
}

impl App {
    pub fn logo_url(&self) -> Option<&str> {
        self.logo.as_deref().filter(|s| s.starts_with("http"))
    }
    pub fn download_url(&self) -> String {
        for u in [&self.dl_a, &self.dl_b] {
            if let Some(s) = u {
                if s.starts_with("http") {
                    return s.clone();
                }
            }
        }
        format!("{}/dlservice/download?softid={}", BASE, self.id)
    }
    pub fn score_text(&self) -> String {
        match &self.score {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    }
    pub fn subtitle(&self) -> String {
        self.introduction.as_deref().unwrap_or("").trim().to_string()
    }
    pub fn meta(&self) -> String {
        let mut v = Vec::new();
        if let Some(s) = &self.size {
            if !s.trim().is_empty() {
                v.push(s.trim().to_string());
            }
        }
        if let Some(s) = &self.downloads {
            if !s.trim().is_empty() {
                v.push(format!("{} 安装", s.trim()));
            }
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
    #[serde(default)]
    pub name: String,
    #[serde(rename = "appClassList", default)]
    pub list: Vec<ClassItem>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Detail {
    #[serde(rename = "softID", default)]
    pub id: String,
    #[serde(rename = "softName", default)]
    pub name: String,
    #[serde(rename = "detailInfo", default)]
    pub detail_info: Option<String>,
    #[serde(rename = "whatNew", default)]
    pub what_new: Option<String>,
    #[serde(default)]
    pub developer: Option<String>,
    #[serde(rename = "downloadUrl", default)]
    pub download_url: Option<String>,
    #[serde(rename = "logoFile", default)]
    pub logo: Option<String>,
}

pub struct Payload {
    pub status: i32,
    pub data: serde_json::Value,
}

pub fn payload(bytes: &[u8]) -> Result<Payload, String> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("JSON: {}", e))?;
    Ok(Payload {
        status: v.get("status").and_then(|x| x.as_i64()).unwrap_or(-1) as i32,
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
            Some(serde_json::Value::String(s)) => {
                Some(s == "1" || s.eq_ignore_ascii_case("true"))
            }
            _ => None,
        };
        let a = App {
            id: text("softID").unwrap_or_default(),
            name: text("softName").unwrap_or_default(),
            introduction: text("introduction"),
            version: text("version"),
            size: text("installFileSize"),
            downloads: text("downloadCount"),
            score: o.get("score").cloned(),
            logo: text("logoFile"),
            dl_a: text("downLoadUrl"),
            dl_b: text("downloadUrl"),
            is_advert: is_ad,
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
