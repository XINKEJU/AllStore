//! WinHTTP transport. Uses the system TLS store and the system proxy settings,
//! so no third-party TLS stack or proxy configuration is needed.
use std::ffi::c_void;
use std::io::Write;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

use crate::model::{self, App, ClassGroup, Detail, BASE, CH};

/// Local diagnostic log (never transmitted anywhere).
pub fn log(msg: &str) {
    let dir = std::env::var("TEMP").unwrap_or_else(|_| ".".into());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{}\\mindosshop.log", dir))
    {
        let _ = writeln!(f, "{}", msg);
    }
}

fn wide(s: &str) -> Vec<u16> {
    // WinHttpAddRequestHeaders takes an explicit character count, so the
    // buffer must NOT contain a trailing NUL.
    s.encode_utf16().collect()
}

fn hs(s: &str) -> HSTRING {
    HSTRING::from(s)
}

struct UrlParts {
    host: String,
    path: String,
    port: u16,
    secure: bool,
}

fn split_url(url: &str) -> Result<UrlParts, String> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("unsupported url: {}", url));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(if secure { 443 } else { 80 })),
        None => (authority.to_string(), if secure { 443 } else { 80 }),
    };
    Ok(UrlParts { host, path: path.to_string(), port, secure })
}

/// Perform one request. `sink`, when provided, receives body chunks as they
/// arrive (used for streaming downloads); otherwise the body is returned.
pub fn http(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&[u8]>,
    mut sink: Option<&mut dyn FnMut(&[u8])>,
) -> Result<Vec<u8>, String> {
    unsafe {
        let u = match split_url(url) {
            Ok(v) => v,
            Err(e) => {
                log(&format!("BADURL {}", e));
                return Err(e);
            }
        };
        log(&format!("REQ {} {}:{}{} hdrs={}", method, u.host, u.port, u.path, headers.len()));

        let session = WinHttpOpen(
            &hs("MindosShop/1.0"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session.is_null() {
            log("FAIL WinHttpOpen");
            return Err("WinHttpOpen failed".into());
        }
        let connect = WinHttpConnect(session, &hs(&u.host), u.port, 0);
        if connect.is_null() {
            log("FAIL WinHttpConnect");
            WinHttpCloseHandle(session).ok();
            return Err("WinHttpConnect failed".into());
        }
        let flags = if u.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = WinHttpOpenRequest(
            connect,
            &hs(method),
            &hs(&u.path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        );
        if request.is_null() {
            log("FAIL WinHttpOpenRequest");
            WinHttpCloseHandle(connect).ok();
            WinHttpCloseHandle(session).ok();
            return Err("WinHttpOpenRequest failed".into());
        }
        let _ = WinHttpSetTimeouts(request, 8000, 10000, 10000, 30000);

        let mut hdr = String::new();
        for (k, v) in headers {
            hdr.push_str(k);
            hdr.push_str(": ");
            hdr.push_str(v);
            hdr.push_str("\r\n");
        }
        if !hdr.is_empty() {
            let w = wide(&hdr);
            if WinHttpAddRequestHeaders(request, &w, WINHTTP_ADDREQ_FLAG_ADD).is_err() {
                log("WARN add headers failed");
            }
        }

        let (ptr, len) = match body {
            Some(b) => (Some(b.as_ptr() as *const c_void), b.len() as u32),
            None => (None, 0),
        };
        if let Err(e) = WinHttpSendRequest(request, None, ptr, len, len, 0) {
            log(&format!("FAIL WinHttpSendRequest {}", e));
            WinHttpCloseHandle(request).ok();
            WinHttpCloseHandle(connect).ok();
            WinHttpCloseHandle(session).ok();
            return Err(format!("WinHttpSendRequest: {}", e));
        }
        if let Err(e) = WinHttpReceiveResponse(request, std::ptr::null_mut()) {
            log(&format!("FAIL WinHttpReceiveResponse {}", e));
            WinHttpCloseHandle(request).ok();
            WinHttpCloseHandle(connect).ok();
            WinHttpCloseHandle(session).ok();
            return Err(format!("WinHttpReceiveResponse: {}", e));
        }

        let mut result = Vec::new();
        loop {
            let mut available = 0u32;
            match WinHttpQueryDataAvailable(request, &mut available) {
                Ok(()) if available > 0 => {}
                Ok(()) => break,
                Err(e) => {
                    log(&format!("WARN QueryDataAvailable {}", e));
                    break;
                }
            }
            let mut buf = vec![0u8; available as usize];
            let mut read = 0u32;
            if let Err(e) = WinHttpReadData(
                request,
                buf.as_mut_ptr() as *mut c_void,
                available,
                &mut read,
            ) {
                log(&format!("WARN ReadData {}", e));
                break;
            }
            buf.truncate(read as usize);
            if buf.is_empty() {
                break;
            }
            match &mut sink {
                Some(f) => f(&buf),
                None => result.extend_from_slice(&buf),
            }
        }
        WinHttpCloseHandle(request).ok();
        WinHttpCloseHandle(connect).ok();
        WinHttpCloseHandle(session).ok();
        log(&format!("RESP {} bytes={}", u.path, result.len()));
        if result.len() < 300 && sink.is_none() {
            log(&format!("BODY {}", String::from_utf8_lossy(&result)));
        }
        Ok(result)
    }
}

fn identity_headers(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("token", token.to_string()),
        ("vt", "0".into()),
        ("productCode", "2".into()),
        ("clientType", "1".into()),
        ("h5version", CH.into()),
        ("cpuVendor", String::new()),
        ("h5need", "1111111111111111".into()),
        ("Referer", "https://pc.lenovomm.cn/".into()),
        ("Origin", "https://pc.lenovomm.cn".into()),
        (
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/124.0.0.0 Safari/537.36".into(),
        ),
    ]
}

/// Register a fresh client token. The minimal legal device profile is enough —
/// no serial number, UUID or MAC is collected or transmitted.
pub fn register() -> Result<String, String> {
    let body = format!(
        r#"{{"ch":"{}","mac":"","mt":"","os":"win11","osBit":"64","sn":"","uuid":"","cpuVendor":"","cpuIdentifier":"","osVersion":"10.0.26300"}}"#,
        CH
    );
    let url = format!("{}/appbase/token/register", BASE);
    let resp = http(
        "POST",
        &url,
        &[("Content-Type", "application/json".into())],
        Some(body.as_bytes()),
        None,
    )?;
    let p = match model::payload(&resp) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("FAIL register payload {}", e));
            return Err(e);
        }
    };
    let token = p
        .data
        .as_str()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("register returned status {} with no token", p.status))?;
    log(&format!("TOKEN ok len={} status={}", token.len(), p.status));
    Ok(token)
}

fn get(token: &str, path: &str) -> Result<serde_json::Value, String> {
    let url = format!("{}{}", BASE, path);
    let resp = http("GET", &url, &identity_headers(token), None, None)?;
    let p = match model::payload(&resp) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("FAIL get payload {} head={}", e, String::from_utf8_lossy(&resp[..resp.len().min(120)])));
            return Err(e);
        }
    };
    log(&format!("GET {} status={} dataLen={}", path, p.status, p.data.to_string().len()));
    if p.status != 0 {
        return Err(format!("api status {}", p.status));
    }
    Ok(p.data)
}

pub fn classes(token: &str) -> Result<Vec<ClassGroup>, String> {
    let v = get(token, "/appstorecontents/class/all_classes_list")?;
    let out = model::parse_classes(&v);
    log(&format!("CLASSES groups={}", out.len()));
    Ok(out)
}

pub fn class_apps(token: &str, code: &str, class_id: &str, limit: u32) -> Result<Vec<App>, String> {
    let path = format!(
        "/appstorecontents/class/class_apps_list?code={}&id={}&skip=0&limit={}&refresh=true",
        code, class_id, limit
    );
    let v = get(token, &path)?;
    let out = model::parse_apps(&v);
    log(&format!("APPS class={} id={} count={}", code, class_id, out.len()));
    Ok(out)
}

#[allow(dead_code)]
pub fn rank_apps(token: &str, limit: u32) -> Result<Vec<App>, String> {
    let path = format!(
        "/appstorecontents/rank/rank_apps?code=ranklist%3AweekDownloadRank&skip=0&limit={}",
        limit
    );
    Ok(model::parse_apps(&get(token, &path)?))
}

pub fn search(token: &str, keyword: &str) -> Result<Vec<App>, String> {
    let path = format!(
        "/searchcontent/search/v4.0/search_apps?softName={}&skip=0&limit=36",
        model::urlencode(keyword)
    );
    let v = get(token, &path)?;
    let out = model::parse_apps(&v);
    log(&format!("SEARCH {} count={}", keyword, out.len()));
    Ok(out)
}

pub fn details(token: &str, id: &str) -> Result<Detail, String> {
    let path = format!(
        "/appstorecontents/app/details_new?softid={}&bizInfo=%7B%7D",
        model::urlencode(id)
    );
    let data = get(token, &path)?;
    serde_json::from_value::<Detail>(data).map_err(|e| e.to_string())
}

/// Fetch a binary resource (app icon or installer). `sink` receives chunks.
pub fn fetch_binary(url: &str, sink: Option<&mut dyn FnMut(&[u8])>) -> Result<Vec<u8>, String> {
    http("GET", url, &[], None, sink)
}
