//! WinHTTP transport. Uses the system TLS store and the system proxy settings,
//! so no third-party TLS stack or proxy configuration is needed.
//!
//! The store's download endpoint answers with a 302 to a plain-HTTP CDN host.
//! WinHTTP refuses that downgrade by default, so redirects are handled here
//! explicitly: status codes and Content-Length are checked, and identity
//! headers are never forwarded to a redirect target.
use std::ffi::c_void;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Networking::WinHttp::*;
use windows::core::{HSTRING, PCWSTR};

use crate::model::{self, App, BASE, CH, ClassGroup, Detail};

/// Local diagnostic log (never transmitted anywhere). Bounded so a long
/// session cannot grow it without limit; response bodies are never logged.
pub fn log(msg: &str) {
    let dir = std::env::var("TEMP").unwrap_or_else(|_| ".".into());
    let path = format!("{}\\mindosshop.log", dir);
    if let Ok(md) = std::fs::metadata(&path)
        && md.len() > 1_048_576
    {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{} {}", ts, msg);
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

#[derive(Clone)]
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
        return Err(format!("不支持的链接协议：{}", &url[..url.len().min(48)]));
    };
    let rest = rest.split('#').next().unwrap_or(rest);
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let default_port = if secure { 443 } else { 80 };
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let Some(end) = inner.find(']') else {
            return Err("链接中的 IPv6 地址格式错误".into());
        };
        let h = inner[..end].to_string();
        let p = inner[end + 1..]
            .strip_prefix(':')
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(default_port);
        (h, p)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(default_port)),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err("链接缺少主机名".into());
    }
    // CDN object names occasionally contain spaces; WinHTTP wants them encoded.
    let path = path.replace(' ', "%20");
    Ok(UrlParts {
        host,
        path,
        port,
        secure,
    })
}

/// Turn a Location header into an absolute URL, including protocol-relative,
/// root-relative and path-relative forms.
fn resolve_location(base: &str, loc: &str) -> String {
    let loc = loc.trim();
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    let Some(scheme_end) = base.find("://").map(|i| i + 3) else {
        return loc.to_string();
    };
    let authority_end = base[scheme_end..]
        .find('/')
        .map(|i| scheme_end + i)
        .unwrap_or(base.len());
    let origin = &base[..authority_end];
    if let Some(rest) = loc.strip_prefix("//") {
        return format!("{}{}", &base[..scheme_end], rest);
    }
    if loc.starts_with('/') {
        return format!("{}{}", origin, loc);
    }
    let path = &base[authority_end..];
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    format!("{}{}/{}", origin, dir, loc)
}

/// Close every WinHTTP handle even when an early `?` returns.
struct Handles(Vec<*mut c_void>);

impl Handles {
    fn new() -> Self {
        Self(Vec::with_capacity(3))
    }
    fn add(&mut self, h: *mut c_void) -> Result<*mut c_void, String> {
        if h.is_null() {
            return Err("WinHTTP 句柄创建失败".into());
        }
        self.0.push(h);
        Ok(h)
    }
}

impl Drop for Handles {
    fn drop(&mut self) {
        unsafe {
            for &h in self.0.iter().rev() {
                WinHttpCloseHandle(h).ok();
            }
        }
    }
}

fn query_status(request: *mut c_void) -> u32 {
    unsafe {
        let mut code = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        let mut index = 0u32;
        let ok = WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut code as *mut u32 as *mut c_void),
            &mut size,
            &mut index,
        )
        .is_ok();
        if ok { code } else { 0 }
    }
}

fn query_header(request: *mut c_void, info: u32) -> Option<String> {
    unsafe {
        let mut size = 0u32;
        let mut index = 0u32;
        // The first call is expected to report ERROR_INSUFFICIENT_BUFFER and
        // fill in the required byte count.
        let _ = WinHttpQueryHeaders(request, info, PCWSTR::null(), None, &mut size, &mut index);
        if size == 0 || size > 64 * 1024 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize / 2) + 1];
        let mut index = 0u32;
        let ok = WinHttpQueryHeaders(
            request,
            info,
            PCWSTR::null(),
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut size,
            &mut index,
        )
        .is_ok();
        if !ok {
            return None;
        }
        let n = (size as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..n]);
        let s = s.trim_end_matches('\0').trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }
}

type Sink<'a> = Option<&'a mut dyn FnMut(&[u8]) -> bool>;
/// Called once as soon as the final 2xx headers arrive, before the body
/// streams. Args: (total_size_of_the_whole_file, is_partial_206). For a 200
/// this is just Content-Length; for a 206 it is `Range.start + Content-Length`
/// so a progress bar keeps the *whole* file as its denominator.
type MetaSink<'a> = Option<&'a mut dyn FnMut(u64, bool)>;

struct OneResp {
    status: u32,
    location: Option<String>,
    content_length: Option<u64>,
    bytes: Vec<u8>,
    /// Bytes read from this response body (a 206 body = remaining range).
    total: u64,
    /// Size of the complete file (206: range start + length; else length).
    whole_total: u64,
}

/// Parse `bytes START-END/TOTAL` from a Content-Range header. Returns
/// (start, total) or None for an unrangeable/malformed response.
pub fn parse_content_range(v: &str) -> Option<(u64, u64)> {
    let v = v.trim();
    let body = v.strip_prefix("bytes ")?;
    let (span, total) = body.rsplit_once('/')?;
    let total: u64 = total.parse().ok()?;
    let start = span.split('-').next()?.trim().parse::<u64>().ok()?;
    Some((start, total))
}

/// WinHTTP reports transport trouble as an HRESULT, which reads as
/// `0x80072EE7` in a download row. Map the codes a user can actually hit onto
/// plain Chinese; the wording deliberately contains `无法解析` / `无法连接` /
/// `超时` because [`crate::downloads::link_retryable`] treats those as "this
/// link is dead, try another source".
fn transport_err(e: &windows::core::Error) -> String {
    friendly_transport_code(e.code().0 as u32)
}

/// Same wording as [`transport_err`], callable from a saved error string that
/// only kept the HRESULT.
pub(crate) fn friendly_transport_code(code: u32) -> String {
    let what = match code {
        0x80072EE7 | 0x800C0005 => "无法解析下载服务器域名",
        0x80072EFD => "无法连接下载服务器",
        0x80072EE2 => "连接下载服务器超时",
        0x80072EFE => "下载连接被中断",
        0x80072F8F | 0x80072F7D => "与下载服务器建立安全连接失败",
        0x80072EE6 => "下载地址格式无效",
        _ => "下载网络错误",
    };
    format!("{}（0x{:08X}）", what, code)
}

/// A single HTTP round trip. Redirect bodies are discarded; only a final 2xx
/// response is streamed into `sink`.
fn one(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    // Headers safe to forward on *every* redirect hop (currently Range).
    // `headers` above are identity headers and stay on hop 0 only.
    carry: &[(&str, String)],
    body: Option<&[u8]>,
    sink: &mut Sink<'_>,
    meta: &mut MetaSink<'_>,
) -> Result<OneResp, String> {
    unsafe {
        let u = split_url(url)?;
        let mut handles = Handles::new();
        let session = handles.add(WinHttpOpen(
            &hs("MindosShop/1.0"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;
        let connect = handles.add(WinHttpConnect(session, &hs(&u.host), u.port, 0))?;
        let flags = if u.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = handles.add(WinHttpOpenRequest(
            connect,
            &hs(method),
            &hs(&u.path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        ))?;

        // We follow redirects ourselves so HTTPS -> HTTP CDN hops are allowed
        // and identity headers never leak to a third-party host.
        let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        let _ = WinHttpSetOption(
            Some(request as *const c_void),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(&policy.to_ne_bytes()),
        );
        let _ = WinHttpSetTimeouts(request, 10000, 15000, 15000, 60000);

        let mut hdr = String::new();
        for (k, v) in headers.iter().chain(carry.iter()) {
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
        WinHttpSendRequest(request, None, ptr, len, len, 0).map_err(|e| transport_err(&e))?;
        WinHttpReceiveResponse(request, std::ptr::null_mut()).map_err(|e| transport_err(&e))?;

        let status = query_status(request);
        let content_length = query_header(request, WINHTTP_QUERY_CONTENT_LENGTH)
            .and_then(|s| s.trim().parse::<u64>().ok());
        if (300..400).contains(&status) {
            return Ok(OneResp {
                status,
                location: query_header(request, WINHTTP_QUERY_LOCATION),
                content_length,
                bytes: Vec::new(),
                total: 0,
                whole_total: 0,
            });
        }
        let success = (200..300).contains(&status);
        // 206 Partial Content: the body is only the remaining range. The
        // complete file size comes from Content-Range (start + length), which
        // is what the progress bar must use as its denominator.
        let partial = status == 206;
        let whole_total = if partial {
            query_header(request, WINHTTP_QUERY_CONTENT_RANGE)
                .and_then(|s| parse_content_range(&s).map(|(_, total)| total))
                .or(content_length)
        } else {
            content_length
        };
        if success
            && let Some(total) = whole_total
            && let Some(f) = meta.as_mut()
        {
            f(total, partial);
        }

        let mut bytes = Vec::new();
        let mut total = 0u64;
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request, &mut available)
                .map_err(|e| format!("查询响应数据失败：{}", e))?;
            if available == 0 {
                break;
            }
            let mut buf = vec![0u8; available as usize];
            let mut read = 0u32;
            WinHttpReadData(
                request,
                buf.as_mut_ptr() as *mut c_void,
                available,
                &mut read,
            )
            .map_err(|e| transport_err(&e))?;
            buf.truncate(read as usize);
            if buf.is_empty() {
                break;
            }
            total += buf.len() as u64;
            if success && let Some(f) = sink.as_mut() {
                if !f(&buf) {
                    return Err("下载被中止".into());
                }
                continue;
            }
            // Error bodies are only kept for diagnostics.
            if !success && bytes.len() >= 4096 {
                continue;
            }
            bytes.extend_from_slice(&buf);
        }
        Ok(OneResp {
            status,
            location: None,
            content_length,
            bytes,
            total,
            whole_total: whole_total.unwrap_or(total),
        })
    }
}

/// Turn a failed HTTP attempt into a line a user can act on. CDN error bodies
/// are JSON blobs (`{"errorcode":-46628,...}`) that mean nothing in a list row,
/// so they are dropped in favour of a short explanation. The status code stays
/// in the text because [`crate::downloads::link_retryable`] keys off it.
pub(crate) fn friendly_http_error(status: u32, body: &[u8]) -> String {
    let base = match status {
        404 | 410 => format!("下载地址已失效（HTTP {}）", status),
        403 => "下载被拒绝（HTTP 403），该地址可能需要登录或限制地区".to_string(),
        429 => "请求过于频繁（HTTP 429），请稍后重试".to_string(),
        500..=599 => format!("下载服务器暂时不可用（HTTP {}）", status),
        400..=499 => format!("下载请求被拒绝（HTTP {}）", status),
        _ => format!("下载失败（HTTP {}）", status),
    };
    // A plain-text body occasionally carries the real reason ("file not
    // found"); JSON or HTML noise is dropped instead of flooding the row.
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    let noisy = text.starts_with('{') || text.starts_with('<') || text.starts_with("<!DOCTYPE");
    if text.is_empty() || noisy || text.chars().count() > 60 {
        base
    } else {
        format!("{}：{}", base, text)
    }
}

pub struct HttpResponse {
    pub bytes: Vec<u8>,
    pub final_url: String,
    pub content_length: Option<u64>,
    /// True when the server honoured a Range request with 206.
    pub resumed: bool,
    /// Size of the complete file (206: range start + length, 200: length).
    pub whole_total: u64,
}

/// Perform a request, following up to six redirects manually.
fn request(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    // Safe-to-forward headers (Range) present on *every* hop, including the
    // final CDN after a redirect. Identity headers above stay on hop 0.
    carry: &[(&str, String)],
    body: Option<&[u8]>,
    mut sink: Sink<'_>,
    mut meta: MetaSink<'_>,
) -> Result<HttpResponse, String> {
    let streaming = sink.is_some();
    let mut current = url.to_string();
    let mut cur_method = method.to_string();
    let mut cur_body = body;
    let empty: [(&str, String); 0] = [];

    for hop in 0..6 {
        let send_headers: &[(&str, String)] = if hop == 0 { headers } else { &empty };
        let r = one(
            &cur_method,
            &current,
            send_headers,
            carry,
            cur_body,
            &mut sink,
            &mut meta,
        )?;
        log(&format!(
            "HTTP {} {} hop={} status={} len={}",
            cur_method,
            split_url(&current).map(|u| u.host).unwrap_or_default(),
            hop,
            r.status,
            r.total
        ));
        if (300..400).contains(&r.status) {
            let Some(loc) = r.location else {
                return Err(format!("服务器返回 HTTP {} 但没有重定向地址", r.status));
            };
            current = resolve_location(&current, &loc);
            // 301/302/303 turn POST into GET and drop the request body.
            if cur_method != "GET" && cur_method != "HEAD" {
                cur_method = "GET".into();
                cur_body = None;
            }
            continue;
        }
        if !(200..300).contains(&r.status) {
            return Err(friendly_http_error(r.status, &r.bytes));
        }
        if streaming {
            if let Some(expected) = r.content_length
                && expected > 0
                && r.total != expected
            {
                return Err(format!(
                    "下载不完整：应为 {} 字节，实际 {} 字节",
                    expected, r.total
                ));
            }
            if r.total == 0 {
                return Err("下载内容为空（服务器未返回安装包）".into());
            }
        }
        return Ok(HttpResponse {
            bytes: r.bytes,
            final_url: current,
            content_length: r.content_length,
            resumed: r.status == 206,
            whole_total: r.whole_total,
        });
    }
    Err("重定向次数过多，已放弃下载".into())
}

/// JSON/API request: body is returned, no streaming sink.
fn http(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    request(method, url, headers, &[], body, None, None).map(|r| r.bytes)
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
    )?;
    let p = model::payload(&resp).map_err(|e| {
        log(&format!("FAIL register payload {}", e));
        e
    })?;
    let token = p
        .data
        .as_str()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("注册失败：status={} 且未返回令牌", p.status))?;
    log(&format!("TOKEN ok len={} status={}", token.len(), p.status));
    Ok(token)
}

fn get(token: &str, path: &str) -> Result<serde_json::Value, String> {
    let url = format!("{}{}", BASE, path);
    let resp = http("GET", &url, &identity_headers(token), None)?;
    let p = model::payload(&resp).map_err(|e| {
        log(&format!("FAIL get payload {} bytes={}", e, resp.len()));
        e
    })?;
    log(&format!(
        "GET {} status={} dataLen={}",
        path.split('?').next().unwrap_or(path),
        p.status,
        p.data.to_string().len()
    ));
    if p.status != 0 {
        return Err(format!("接口返回 status {}", p.status));
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
    log(&format!(
        "APPS class={} id={} count={}",
        code,
        class_id,
        out.len()
    ));
    Ok(out)
}

pub fn search(token: &str, keyword: &str) -> Result<Vec<App>, String> {
    let path = format!(
        "/searchcontent/search/v4.0/search_apps?softName={}&skip=0&limit=36",
        model::urlencode(keyword)
    );
    let v = get(token, &path)?;
    let out = model::parse_apps(&v);
    log(&format!("SEARCH count={}", out.len()));
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

/// HEAD-probe a download link. Used to hide entries whose CDN object is gone:
/// the Tencent list API keeps advertising files it has already deleted.
pub fn url_alive(url: &str) -> bool {
    http("HEAD", url, &[], None).is_ok()
}

/// Fetch a small binary resource (app icon).
pub fn fetch_binary(url: &str) -> Result<Vec<u8>, String> {
    http("GET", url, &[], None)
}

/// Fetch and parse a small JSON document from any host (used by the extra
/// software sources, which do not belong to the Lenovo token API).
pub fn fetch_json(url: &str) -> Result<serde_json::Value, String> {
    let body = fetch_binary(url)?;
    serde_json::from_slice(&body).map_err(|e| format!("JSON：{}", e))
}

/// GET with caller-chosen headers, keeping the final URL. GitHub's
/// `releases/latest` answers with a redirect whose path *is* the tag, so the
/// resolved address matters as much as the body.
pub fn get_with(url: &str, headers: &[(&str, String)]) -> Result<HttpResponse, String> {
    request("GET", url, headers, &[], None, None, None)
}

/// Same, parsed as JSON (GitHub's API rejects requests with no User-Agent).
pub fn get_json_with(url: &str, headers: &[(&str, String)]) -> Result<serde_json::Value, String> {
    let body = get_with(url, headers)?.bytes;
    serde_json::from_slice(&body).map_err(|e| format!("JSON：{}", e))
}

/// Metadata returned after a streamed package download. The final URL lets the
/// caller pick a truthful file extension instead of blindly writing `.exe`.
pub struct Fetch {
    pub final_url: String,
    /// Bytes delivered by *this* response (a 206 body = remaining range).
    pub size: u64,
    /// Size of the complete file across all ranges.
    pub total: u64,
    /// True when the server honoured `Range` with 206 Partial Content.
    pub resumed: bool,
}

/// Stream an installer to `sink`, reporting the whole-file size as soon as the
/// final response headers arrive so the UI can draw a real percentage from
/// byte 0. `meta` receives (whole_file_size, is_partial_206). Returning `false`
/// from the sink aborts the transfer.
#[allow(dead_code)] // convenience wrapper, exercised by the live tests
pub fn fetch_binary_stream(
    url: &str,
    sink: &mut dyn FnMut(&[u8]) -> bool,
    meta: &mut dyn FnMut(u64, bool),
) -> Result<Fetch, String> {
    fetch_binary_stream_resume(url, 0, sink, meta)
}

/// Same as `fetch_binary_stream` but asks the server for bytes `[from..]`.
/// A `Range` header is attached to *every* redirect hop so it survives the
/// store's 302-to-CDN dance. Servers that ignore it reply 200 with the full
/// body; `Fetch.resumed` is then false and the caller must overwrite from 0.
pub fn fetch_binary_stream_resume(
    url: &str,
    from: u64,
    sink: &mut dyn FnMut(&[u8]) -> bool,
    meta: &mut dyn FnMut(u64, bool),
) -> Result<Fetch, String> {
    let carry: Vec<(&str, String)> = if from > 0 {
        vec![("Range", format!("bytes={}-", from))]
    } else {
        Vec::new()
    };
    let r = request("GET", url, &[], &carry, None, Some(sink), Some(meta))?;
    Ok(Fetch {
        final_url: r.final_url,
        size: r.content_length.unwrap_or(r.whole_total),
        total: r.whole_total,
        resumed: r.resumed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_redirect_locations() {
        let base = "https://pc-store.lenovomm.cn/dlservice/download?softid=L1";
        assert_eq!(
            resolve_location(base, "http://cdn.example.com/a/b.exe"),
            "http://cdn.example.com/a/b.exe"
        );
        assert_eq!(
            resolve_location(base, "//cdn.example.com/a.exe"),
            "https://cdn.example.com/a.exe"
        );
        assert_eq!(
            resolve_location(base, "/pkg/a.exe"),
            "https://pc-store.lenovomm.cn/pkg/a.exe"
        );
        assert_eq!(
            resolve_location(base, "a.exe"),
            "https://pc-store.lenovomm.cn/dlservice/a.exe"
        );
    }

    #[test]
    fn http_errors_read_like_advice_not_cdn_json() {
        // A dead Tencent CDN answers with a JSON blob; the row must not show it.
        let body = br#"{"errorcode":-46628,"errormsg":"file not found"}"#;
        let msg = friendly_http_error(404, body);
        assert!(msg.contains("下载地址已失效"), "{}", msg);
        assert!(
            msg.contains("HTTP 404"),
            "status code stays for retry logic"
        );
        assert!(
            !msg.contains("errorcode"),
            "raw JSON must not leak: {}",
            msg
        );

        // Plain-text bodies are short and useful enough to keep.
        assert_eq!(
            friendly_http_error(500, b"upstream timeout"),
            "下载服务器暂时不可用（HTTP 500）：upstream timeout"
        );
        // HTML error pages are noise.
        assert_eq!(
            friendly_http_error(403, b"<html><body>denied</body></html>"),
            "下载被拒绝（HTTP 403），该地址可能需要登录或限制地区"
        );
        assert_eq!(friendly_http_error(410, b""), "下载地址已失效（HTTP 410）");
    }

    #[test]
    fn parses_content_range_for_resume() {
        assert_eq!(
            parse_content_range("bytes 1048576-2097151/4194304"),
            Some((1048576, 4194304))
        );
        assert_eq!(parse_content_range("bytes 0-99/100"), Some((0, 100)));
        // An unknown total (`*`) cannot give the progress bar a denominator.
        assert_eq!(parse_content_range("bytes 0-99/*"), None);
        assert_eq!(parse_content_range("garbage"), None);
        assert_eq!(parse_content_range("bytes /100"), None);
    }

    #[test]
    fn splits_urls_with_query_and_ipv6() {
        let u = split_url("https://host.cn:8443/dlservice/download?softid=L1#frag").unwrap();
        assert_eq!(u.host, "host.cn");
        assert_eq!(u.port, 8443);
        assert_eq!(u.path, "/dlservice/download?softid=L1");
        assert!(u.secure);
        let v = split_url("http://[::1]:8080/x y.exe").unwrap();
        assert_eq!(v.host, "::1");
        assert_eq!(v.port, 8080);
        assert_eq!(v.path, "/x%20y.exe");
        assert!(split_url("ftp://host/x").is_err());
    }

    /// Live checks against the real store. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires live network"]
    fn live_download_endpoint_redirects_to_cdn() {
        let url = format!("{}/dlservice/download?softid=L120416", BASE);
        let mut sink: Sink<'_> = None;
        let mut meta: MetaSink<'_> = None;
        let r = one("GET", &url, &[], &[], None, &mut sink, &mut meta).expect("request failed");
        assert_eq!(r.status, 302, "expected redirect, got {}", r.status);
        let loc = r.location.expect("missing Location header");
        assert!(loc.starts_with("http://"), "unexpected Location: {}", loc);
        assert!(loc.contains(".exe"), "unexpected Location: {}", loc);
    }

    #[test]
    #[ignore = "requires live network"]
    fn live_tencent_stream_delivers_bytes() {
        let url =
            "https://webcdn.m.qq.com/spcmgr/download/28160-110820-XYLinkClient-3.3.30.9093.exe";
        let mut got = 0usize;
        let mut total = 0u64;
        let r = fetch_binary_stream_resume(
            url,
            0,
            &mut |chunk: &[u8]| -> bool {
                got += chunk.len();
                got < 512 * 1024 // stop after half a megabyte
            },
            &mut |n: u64, _partial: bool| total = n,
        );
        println!(
            "stream result={:?} bytes={} total={}",
            r.is_ok(),
            got,
            total
        );
        assert!(got > 100_000, "no bytes arrived");
    }

    #[test]
    #[ignore = "requires live network"]
    fn live_print_lenovo_classes() {
        let token = register().expect("register");
        let groups = classes(&token).expect("classes");
        for g in &groups {
            for c in &g.list {
                println!("CLASS code={} id={} name={}", g.code, c.id, c.name);
            }
        }
    }

    #[test]
    #[ignore = "requires live network"]
    fn live_download_follows_redirect_and_returns_pe() {
        let url = format!("{}/dlservice/download?softid=L120416", BASE);
        let mut head = Vec::new();
        let result = request(
            "GET",
            &url,
            &[],
            &[],
            None,
            Some(&mut |chunk: &[u8]| -> bool {
                if head.is_empty() {
                    head.extend_from_slice(&chunk[..chunk.len().min(2)]);
                }
                false // abort after the first chunk; we only need the signature
            }),
            None,
        );
        assert!(result.is_err(), "sink abort should surface as an error");
        assert_eq!(&head, b"MZ", "first bytes are not a PE executable");
    }
}
