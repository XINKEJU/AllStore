//! Download centre: a real task queue with live progress, speed, cancel,
//! retry and "open containing folder". Every visible surface (app cards,
//! detail page, status bar, this page) reads the same task list.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use crate::app::{
    AppState, GAP, HIT_DL_CANCEL, HIT_DL_CLEAR, HIT_DL_INSTALL, HIT_DL_OPEN, HIT_DL_PAUSE,
    HIT_DL_REMOVE, HIT_DL_RETRY, NAV_W, PAD, Payload, STATUS_H, TOP_H, WM_APP_DL_DONE,
    WM_APP_DL_PROGRESS, WM_APP_FALLBACK, draw_button, in_rect, post,
};
use crate::gfx::{self, Color, Gfx, Palette};
use crate::net;
use crate::updates_ui;

/// Rewrite a legacy error string (`服务器返回 HTTP 404（{"errorcode":…}）`) into
/// the short wording the list rows expect. Anything already readable, or any
/// non-HTTP failure (disk full, cancelled), is passed through untouched.
pub(crate) fn sanitize_error(msg: &str) -> String {
    const MARK: &str = "服务器返回 HTTP ";
    if let Some(pos) = msg.find(MARK) {
        let digits: String = msg[pos + MARK.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(code) = digits.parse::<u32>() {
            return crate::net::friendly_http_error(code, b"");
        }
    }
    // 0.9.1 stored WinHTTP failures verbatim ("发送请求失败：0x80072EE7").
    if let Some(pos) = msg.find("0x") {
        let hex: String = msg[pos + 2..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if hex.len() == 8
            && let Ok(code) = u32::from_str_radix(&hex, 16)
        {
            return crate::net::friendly_transport_code(code);
        }
    }
    msg.to_string()
}

/// Whether a failed transfer is worth re-resolving on another source: a static
/// third-party link is worth exactly one retry, a per-request Lenovo link is
/// not (its URL is generated fresh and would just fail again).
pub(crate) fn link_retryable(already_failed_over: bool, url: &str, err: &str) -> bool {
    if already_failed_over || url.contains("lenovomm.cn") {
        return false;
    }
    // Third-party links die in two ways: the CDN answers 4xx/5xx, or the host
    // stops resolving / refuses the connection (Tencent's older objects are
    // mostly the second kind). Local failures — disk full, paused, cancelled —
    // carry none of these markers and must not trigger a source switch.
    const LINK_DEAD: [&str; 7] = [
        "HTTP 4",
        "HTTP 5",
        "无法解析",
        "无法连接",
        "超时",
        "连接被中断",
        "下载地址格式无效",
    ];
    LINK_DEAD.iter().any(|m| err.contains(m))
}

pub(crate) const DL_ROW_H: f32 = 100.0;
/// How many transfers may hold a slot at once. Three keeps a fast line busy
/// without opening a dozen sockets for a batch of updates.
pub(crate) const MAX_CONCURRENT: usize = 3;
/// Automatic retries per task after a transport failure (2 s, 4 s backoff).
const MAX_RETRIES: u32 = 2;
/// One sample every 500 ms -> a 36 second rolling window.
pub(crate) const SPEED_SAMPLES: usize = 72;
pub(crate) const DL_CHART_TOP: f32 = TOP_H + 96.0;
pub(crate) const DL_CHART_H: f32 = 172.0;
pub(crate) const DL_ROWS_TOP: f32 = DL_CHART_TOP + DL_CHART_H + GAP;
const _: () = assert!(DL_ROWS_TOP >= DL_CHART_TOP + DL_CHART_H);

#[derive(Clone, Copy, Default, Debug)]
pub struct SpeedSample {
    pub net: f64,
    pub disk: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DlState {
    Queued,
    Downloading,
    /// Manually paused or restored from disk after a restart. Resuming keeps
    /// the bytes already written to the `.part` file (HTTP Range).
    Paused,
    Done,
    Failed,
    Canceled,
}

pub struct DlTask {
    pub id: u64,
    pub name: String,
    pub url: String,
    pub logo: Option<String>,
    pub state: DlState,
    pub done: u64,
    pub total: u64,
    /// Smoothed bytes per second received from the network.
    pub speed: f64,
    /// Smoothed bytes per second actually committed to disk.
    pub disk_speed: f64,
    pub path: Option<String>,
    pub error: Option<String>,
    pub cancel: Arc<AtomicBool>,
    /// A dead third-party link was already replaced by a Lenovo one; a second
    /// failure is final so we never loop between sources.
    pub failover: bool,
}

impl DlTask {
    pub fn frac(&self) -> f32 {
        match self.state {
            DlState::Done => 1.0,
            DlState::Downloading | DlState::Queued if self.total == 0 => 0.10,
            DlState::Paused if self.total == 0 => 0.0,
            _ if self.total == 0 => 0.0,
            _ => (self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32,
        }
    }
    pub fn percent(&self) -> String {
        if self.total == 0 && self.state == DlState::Downloading {
            return "…".into();
        }
        format!("{}%", (self.frac() * 100.0).round() as i32)
    }
}

pub fn fmt_size(n: u64) -> String {
    const K: f64 = 1024.0;
    let b = n as f64;
    if b < K {
        format!("{} B", n)
    } else if b < K * K {
        format!("{:.1} KB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MB", b / (K * K))
    } else {
        format!("{:.2} GB", b / (K * K * K))
    }
}

pub fn fmt_speed(bps: f64) -> String {
    if bps <= 0.0 {
        String::new()
    } else {
        format!("{}/s", fmt_size(bps.max(1.0) as u64))
    }
}

/// Round a peak up to the next power-of-two binary step (64 KiB, 128 KiB,
/// 256 KiB …). `fmt_size` is 1024-based, so decimal 1/2/5 steps would print
/// awkward labels like "4.8 MB/s" at the top gridline.
pub(crate) fn nice_scale(v: f64) -> f64 {
    const FLOOR: f64 = 64.0 * 1024.0;
    const CEILING: f64 = 1024.0 * 1024.0 * 1024.0;
    if !v.is_finite() || v <= 0.0 {
        return FLOOR;
    }
    let mut step = FLOOR;
    while step < v && step < CEILING {
        step *= 2.0;
    }
    step
}

/// Horizontal anchors for the three header columns of the speed panel.
fn speed_header_columns(l: f32, r: f32) -> (f32, f32, f32) {
    let pw = r - l;
    let c1 = (pw * 0.28).max(130.0);
    let c2 = (pw * 0.26).max(120.0);
    let x1 = l;
    let x2 = l + c1;
    let x3 = (x2 + c2).min(r - 180.0).max(x2 + 8.0);
    (x1, x2, x3)
}

/// A rounded progress bar. `color` lets finished/failed tasks keep their own
/// identity instead of every bar looking "in progress".
pub fn progress_bar(g: &mut Gfx, p: Palette, r: D2D_RECT_F, frac: f32, color: Color) {
    if r.right <= r.left || r.bottom <= r.top {
        return;
    }
    let rad = (r.bottom - r.top) / 2.0;
    g.round(r, rad, p.subtle);
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        let w = ((r.right - r.left) * f).max(r.bottom - r.top);
        g.round(gfx::rect(r.left, r.top, r.left + w, r.bottom), rad, color);
    }
}

/// Emits throttled progress from the worker thread. Posting on every
/// WinHttpReadData chunk would flood the message queue for fast connections;
/// 80 ms or 1% of the file is enough for a smooth bar.
pub struct Progress {
    id: Option<u64>,
    hwnd: usize,
    name: String,
    done: u64,
    total: u64,
    speed: f64,
    disk_speed: f64,
    written: u64,
    last_written: u64,
    tick: Instant,
    last_done: u64,
    last_pct: i32,
    pub posts: usize,
}

impl Progress {
    pub fn new(id: Option<u64>, hwnd: usize, name: &str) -> Self {
        Self {
            id,
            hwnd,
            name: name.to_string(),
            done: 0,
            total: 0,
            speed: 0.0,
            disk_speed: 0.0,
            written: 0,
            last_written: 0,
            tick: Instant::now(),
            last_done: 0,
            last_pct: -1,
            posts: 0,
        }
    }

    /// Bytes confirmed by `write_all`. Counting over wall-clock windows (not
    /// the duration of the write call) keeps disk throughput comparable to
    /// network throughput: a slow disk makes this number lag behind.
    pub fn written(&mut self, n: usize) {
        self.written += n as u64;
    }

    pub fn set_total(&mut self, total: u64) {
        self.total = total;
        self.emit(true);
    }

    /// Seed the counter with already-persisted bytes so a resumed download
    /// starts its bar at the correct percentage instead of jumping from 0.
    pub fn set_done(&mut self, done: u64) {
        self.done = done;
        self.written = done;
        self.last_done = done;
        self.last_written = done;
        self.last_pct = -1;
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn add(&mut self, n: usize) {
        self.done += n as u64;
        let now = Instant::now();
        let elapsed = now.duration_since(self.tick).as_secs_f64();
        if elapsed >= 0.08 {
            let delta = self.done.saturating_sub(self.last_done);
            let delta_disk = self.written.saturating_sub(self.last_written);
            let instant = if elapsed > 0.0 {
                delta as f64 / elapsed
            } else {
                0.0
            };
            let instant_disk = if elapsed > 0.0 {
                delta_disk as f64 / elapsed
            } else {
                0.0
            };
            self.speed = if self.speed == 0.0 {
                instant
            } else {
                self.speed * 0.65 + instant * 0.35
            };
            self.disk_speed = if self.disk_speed == 0.0 {
                instant_disk
            } else {
                self.disk_speed * 0.65 + instant_disk * 0.35
            };
            self.tick = now;
            self.last_done = self.done;
            self.last_written = self.written;
        }
        let pct = self
            .done
            .checked_mul(100)
            .and_then(|v| v.checked_div(self.total))
            .map(|v| v as i32)
            .unwrap_or(-1);
        if pct >= 0 && pct != self.last_pct {
            self.last_pct = pct;
            self.emit(false);
        }
    }

    fn emit(&mut self, _force: bool) {
        self.posts += 1;
        if self.hwnd == 0 {
            return;
        }
        match self.id {
            Some(id) => post(
                self.hwnd,
                WM_APP_DL_PROGRESS,
                Payload::DlProgress {
                    id,
                    done: self.done,
                    total: self.total,
                    speed: self.speed,
                    disk: self.disk_speed,
                },
            ),
            None => post(
                self.hwnd,
                crate::app::WM_APP_STATUS,
                Payload::Status(format!("正在下载 {} … {}", self.name, fmt_size(self.done))),
            ),
        }
    }
}

pub fn open_folder(path: &str) {
    // Selects the file in Explorer without executing it — the low-risk
    // secondary action for users who want the installer file itself.
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path))
        .spawn();
}

/// Launch a downloaded package through the shell. `ShellExecuteW("open")`
/// respects the file association and, unlike `CreateProcess`, honours the
/// installer's manifest: a package that requests elevation triggers the normal
/// UAC prompt, `.msi` goes through msiexec, and archives open in Explorer.
///
/// Returns `Ok(())` when the shell accepted the request (the installer has been
/// *started*, not silently installed), or a human-readable error. Only paths
/// produced by our own verified downloader reach this function.
pub fn run_installer(path: &str) -> Result<(), String> {
    if path.is_empty() || !std::path::Path::new(path).is_file() {
        return Err("安装包文件不存在，可能已被移动或删除".into());
    }
    let file = HSTRING::from(path);
    let exe = w!("open");
    // ShellExecuteW returns an HINSTANCE whose numeric value > 32 means
    // success; values <= 32 are Win32 error codes (it does not use HRESULT).
    let ret = unsafe {
        ShellExecuteW(
            None,
            exe,
            &file,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = ret.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(shell_error(code))
    }
}

/// Translate the small integer error codes documented for ShellExecute into a
/// message an end user can act on. Unknown codes keep the raw number so a
/// support report is still useful.
fn shell_error(code: isize) -> String {
    match code {
        0 => "系统内存不足，无法启动安装程序".into(),
        2 => "找不到安装包文件".into(),
        3 => "安装包的路径无效".into(),
        5 => "系统拒绝访问该安装包".into(),
        8 => "系统资源不足，无法启动安装程序".into(),
        11 => "安装包格式无效，不是可执行的程序".into(),
        26 => "安装包正被其他程序占用，请稍后重试".into(),
        27 => "缺少文件关联，无法打开该安装包".into(),
        31 => "没有关联的程序可以打开该安装包".into(),
        _ => format!("系统未能启动安装程序（错误码 {}）", code),
    }
}

impl AppState {
    /// Start the installer for a finished download. Only a `Done` task with a
    /// verified path can reach here, so we never launch a half-written `.part`.
    /// The install itself runs in a separate process; a successful return means
    /// it was *started*, and the normal UAC prompt may still be pending.
    pub fn install_task(&mut self, id: u64) {
        let Some(t) = self.downloads.iter().find(|t| t.id == id) else {
            return;
        };
        if t.state != DlState::Done {
            return;
        }
        let (name, path) = (t.name.clone(), t.path.clone());
        let result = path
            .as_deref()
            .map(run_installer)
            .unwrap_or_else(|| Err("安装包路径已丢失".into()));
        self.status = match result {
            Ok(()) => format!(
                "已启动 {} 的安装程序  ·  请按系统提示完成安装，完成后列表会自动刷新",
                name
            ),
            Err(e) => format!("无法启动 {} 的安装程序：{}", name, e),
        };
        self.invalidate();
    }

    /// Reuse an existing task instead of downloading the same app twice.
    /// A Paused task is reused too, so re-clicking "获取" on a half-downloaded
    /// app resumes it instead of creating a duplicate.
    pub fn new_download(&mut self, name: &str, url: &str, logo: Option<String>) -> u64 {
        if let Some(t) = self.downloads.iter().rev().find(|t| {
            t.name == name
                && matches!(
                    t.state,
                    DlState::Queued | DlState::Downloading | DlState::Paused
                )
        }) {
            return t.id;
        }
        self.next_dl_id += 1;
        let id = self.next_dl_id;
        self.downloads.push(DlTask {
            id,
            name: name.to_string(),
            url: url.to_string(),
            logo,
            state: DlState::Queued,
            done: 0,
            total: 0,
            speed: 0.0,
            disk_speed: 0.0,
            path: None,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
            failover: false,
        });
        id
    }

    /// Queue a task and let the pump decide when it actually runs. Keeping
    /// the concurrency cap here (instead of "one thread per click") is what
    /// stops a batch of twenty clicks from saturating the line.
    pub fn start_task(&mut self, id: u64) {
        let Some(t) = self.downloads.iter_mut().find(|t| t.id == id) else {
            return;
        };
        // Already transferring: nothing to do. A Queued task must still fall
        // through to `pump()`, otherwise a freshly created task (which is born
        // Queued) would sit in the list forever waiting for someone to kick the
        // scheduler.
        if t.state == DlState::Downloading {
            return;
        }
        let resume_from = updates_ui::partial_bytes(&t.name, &t.url);
        t.state = DlState::Queued;
        t.done = resume_from;
        // Keep the known total across a resume so the bar doesn't collapse;
        // a fresh download probes it from the first response header.
        if resume_from == 0 {
            t.total = 0;
        }
        t.speed = 0.0;
        t.disk_speed = 0.0;
        t.error = None;
        t.cancel.store(false, Ordering::Relaxed);
        self.save_downloads();
        self.pump();
    }

    /// Transfers currently holding a download slot (queued tasks do not).
    pub fn running_count(&self) -> usize {
        self.downloads
            .iter()
            .filter(|t| t.state == DlState::Downloading)
            .count()
    }

    /// Start queued tasks while there is a free slot. Called after queueing,
    /// after a task finishes and after a pause frees its slot.
    pub(crate) fn pump(&mut self) {
        while self.running_count() < MAX_CONCURRENT {
            let Some(id) = self
                .downloads
                .iter()
                .find(|t| t.state == DlState::Queued)
                .map(|t| t.id)
            else {
                break;
            };
            self.spawn_task(id);
        }
        // Recompute the status line only when something is actually waiting.
        let waiting = self
            .downloads
            .iter()
            .filter(|t| t.state == DlState::Queued)
            .count();
        if waiting > 0 {
            self.status = format!("{} 个下载正在排队，完成后自动开始", waiting);
            self.invalidate();
        }
    }

    fn spawn_task(&mut self, id: u64) {
        let Some(t) = self.downloads.iter_mut().find(|t| t.id == id) else {
            return;
        };
        let resume_from = updates_ui::partial_bytes(&t.name, &t.url);
        t.state = DlState::Downloading;
        t.done = resume_from;
        t.speed = 0.0;
        t.disk_speed = 0.0;
        t.error = None;
        t.cancel.store(false, Ordering::Relaxed);
        let (name, url, cancel) = (t.name.clone(), t.url.clone(), t.cancel.clone());
        let h = self.hwnd_usize();
        self.status = if resume_from > 0 {
            format!("继续下载 {}（已保存 {}）…", name, fmt_size(resume_from))
        } else {
            format!("开始下载 {} …", name)
        };
        self.save_downloads();
        self.invalidate();
        std::thread::spawn(move || {
            // Transient failures (server hiccup, Wi-Fi roam) are common for
            // multi-hundred-MB installers. Retry twice with backoff; every
            // attempt resumes from the `.part` left by the previous one, so a
            // retry never re-downloads what already landed on disk.
            let mut attempt = 0u32;
            let result = loop {
                let already = updates_ui::partial_bytes(&name, &url);
                let r = updates_ui::download_package_ex(
                    &name,
                    &url,
                    h,
                    Some(id),
                    Some(cancel.clone()),
                    already > 0,
                );
                if r.is_ok() || cancel.load(Ordering::Relaxed) || attempt >= MAX_RETRIES {
                    break r;
                }
                attempt += 1;
                let wait = 2u64 * attempt as u64;
                post(
                    h,
                    crate::app::WM_APP_STATUS,
                    Payload::Status(format!(
                        "{} 中断，{} 秒后自动重试（{}/{}）…",
                        name, wait, attempt, MAX_RETRIES
                    )),
                );
                std::thread::sleep(std::time::Duration::from_secs(wait));
            };
            post(h, WM_APP_DL_DONE, Payload::DlDone { id, result });
        });
    }

    /// Persist the durable part of every task. Called whenever state changes
    /// (start / pause / finish / remove). Cheap: a handful of small JSON rows.
    pub(crate) fn save_downloads(&self) {
        use crate::persist::{self, SavedState, TaskSnapshot};
        let snap: Vec<TaskSnapshot> = self
            .downloads
            .iter()
            .map(|t| TaskSnapshot {
                name: t.name.clone(),
                url: t.url.clone(),
                logo: t.logo.clone(),
                state: SavedState::from(t.state),
                done: updates_ui::partial_bytes(&t.name, &t.url),
                total: t.total,
                path: t.path.clone(),
                error: t.error.clone(),
            })
            .collect();
        persist::save(&snap);
    }

    /// Rebuild tasks from `downloads.json` on startup. A "Downloading" state
    /// saved by a killed process becomes `Paused` (never auto-reconnect), and
    /// a done task whose file vanished is marked failed with a clear message.
    pub(crate) fn restore_downloads(&mut self) {
        let snaps = crate::persist::load();
        if snaps.is_empty() {
            return;
        }
        let mut restored = 0usize;
        let mut resumed_bytes = 0u64;
        for s in snaps {
            if s.name.is_empty() || !s.url.starts_with("http") {
                continue;
            }
            self.next_dl_id += 1;
            let mut state = DlState::from(s.state);
            let mut path = s.path.clone();
            // Snapshots written by 0.9.1 and earlier keep the raw CDN body in
            // `error`; rewrite them on the way in so an upgrade immediately
            // shows a readable line instead of a JSON blob.
            let mut error = s.error.clone().map(|e| sanitize_error(&e));
            let saved_done = updates_ui::partial_bytes(&s.name, &s.url);
            match state {
                DlState::Done => {
                    let alive = path
                        .as_deref()
                        .is_some_and(|p| std::path::Path::new(p).is_file());
                    if !alive {
                        state = DlState::Failed;
                        error = Some("安装包已不在 Downloads 中，请重新下载".into());
                        path = None;
                    }
                }
                DlState::Paused | DlState::Downloading | DlState::Queued => {
                    if saved_done > 0 {
                        state = DlState::Paused;
                        resumed_bytes += saved_done;
                    } else {
                        state = DlState::Canceled;
                    }
                }
                _ => {}
            }
            self.downloads.push(DlTask {
                id: self.next_dl_id,
                name: s.name,
                url: s.url,
                logo: s.logo,
                state,
                done: saved_done,
                total: if saved_done > 0 { s.total } else { 0 },
                speed: 0.0,
                disk_speed: 0.0,
                path,
                error,
                cancel: Arc::new(AtomicBool::new(false)),
                failover: false,
            });
            restored += 1;
        }
        if restored > 0 {
            Self::net_log_restore(restored, resumed_bytes);
        }
    }

    /// Pause a running task: flip the cancel flag, let the worker stop, and
    /// keep its `.part` bytes. The state becomes `Paused` on the DONE callback.
    pub fn pause_task(&mut self, id: u64) {
        if let Some(t) = self.downloads.iter_mut().find(|t| t.id == id)
            && matches!(t.state, DlState::Downloading | DlState::Queued)
        {
            let name = t.name.clone();
            t.cancel.store(true, Ordering::Relaxed);
            t.state = DlState::Paused;
            t.speed = 0.0;
            t.disk_speed = 0.0;
            self.status = format!("已暂停 {}，进度已保存，可稍后继续", name);
            self.save_downloads();
            self.pump();
            self.invalidate();
        }
    }

    /// Resume a paused task (or retry a failed/cancelled one) from disk.
    pub fn resume_task(&mut self, id: u64) {
        self.start_task(id);
    }

    fn net_log_restore(restored: usize, resumed_bytes: u64) {
        crate::net::log(&format!(
            "RESTORE tasks={} resumable_bytes={}",
            restored, resumed_bytes
        ));
    }

    pub fn on_dl_progress(&mut self, id: u64, done: u64, total: u64, speed: f64, disk: f64) {
        let Some(t) = self.downloads.iter_mut().find(|t| t.id == id) else {
            return;
        };
        // A batch job flips from "waiting" to "downloading" on its first
        // progress tick, so the page never shows nine bars moving at once.
        if t.state == DlState::Queued {
            t.state = DlState::Downloading;
        } else if t.state != DlState::Downloading {
            return;
        }
        t.done = done;
        if total > 0 {
            t.total = total;
        }
        t.speed = speed;
        t.disk_speed = disk;
        self.invalidate();
    }

    /// Result of the cross-source lookup started when a link died. `None`
    /// means no source could offer a replacement, so the task stays failed
    /// with the original error.
    pub fn on_fallback(&mut self, id: u64, url: Option<String>) {
        let Some(t) = self.downloads.iter_mut().find(|t| t.id == id) else {
            return;
        };
        let name = t.name.clone();
        match url {
            Some(u) if u != t.url => {
                t.url = u;
                t.error = None;
                t.done = 0;
                t.total = 0;
                // Failed (not Queued) so `start_task` does not short-circuit.
                t.state = DlState::Failed;
                self.status = format!("已为 {} 找到替代下载地址，正在重试…", name);
                self.start_task(id);
                self.invalidate();
            }
            Some(_) => {
                self.status = format!("{} 的下载链接仍然不可用", name);
                self.invalidate();
            }
            None => {
                self.status = format!("{} 的下载链接已失效，其他源也没有可用安装包", name);
                self.invalidate();
            }
        }
    }

    pub fn on_dl_done(&mut self, id: u64, result: Result<String, String>) {
        let Some(t) = self.downloads.iter_mut().find(|t| t.id == id) else {
            return;
        };
        match result {
            Ok(path) => {
                t.state = DlState::Done;
                t.path = Some(path.clone());
                t.error = None;
                t.done = t.total.max(t.done);
                t.speed = 0.0;
                t.disk_speed = 0.0;
                // The installer now lives in Downloads; the staging partial
                // is redundant. Remove it after the rename/copy succeeded.
                let _ = std::fs::remove_file(updates_ui::part_path(&t.name, &t.url));
                let short = path.rsplit(['\\', '/']).next().unwrap_or(&path);
                self.status = format!("✓ {} 已下载到 Downloads（{}）", t.name, short);
            }
            Err(e) => {
                // A pause pre-set this to Paused and flipped the cancel flag;
                // keep it Paused so "继续" is offered (bytes are on disk).
                t.state = if t.state == DlState::Paused {
                    DlState::Paused
                } else if t.cancel.load(Ordering::Relaxed) {
                    DlState::Canceled
                } else {
                    DlState::Failed
                };
                t.error = Some(e.clone());
                t.speed = 0.0;
                t.disk_speed = 0.0;
                // A third-party CDN object can disappear while the catalog
                // still advertises it. Ask the Lenovo source for the same app
                // once before giving up.
                let retryable = link_retryable(t.failover, &t.url, &e);
                let (name, id) = (t.name.clone(), t.id);
                self.status = format!("{} 下载未完成：{}", name, e);
                if retryable {
                    if let Some(t) = self.downloads.iter_mut().find(|t| t.id == id) {
                        t.failover = true;
                    }
                    let h = self.hwnd_usize();
                    let token = self.token.clone();
                    self.status = format!("{} 的直链已失效，正在从其他源重试…", name);
                    std::thread::spawn(move || {
                        let found = net::search(&token, &name).ok().and_then(|list| {
                            list.into_iter()
                                .find(|a| !a.download_url().is_empty())
                                .map(|a| a.download_url())
                        });
                        post(h, WM_APP_FALLBACK, Payload::Fallback { id, url: found });
                    });
                }
            }
        }
        // An installer may have just been run from Downloads. Re-scan the
        // uninstall keys so the updates badge does not keep nagging.
        if self.mode == crate::app::Mode::Updates {
            self.refresh_updates_local();
        }
        self.save_downloads();
        // A finished task frees its slot; give it to the next queued job.
        self.pump();
        self.invalidate();
    }

    pub fn cancel_task(&mut self, id: u64) {
        if let Some(t) = self.downloads.iter().find(|t| t.id == id) {
            t.cancel.store(true, Ordering::Relaxed);
            self.status = format!("正在取消 {} 的下载…", t.name);
            self.save_downloads();
            self.invalidate();
        }
    }

    pub fn retry_task(&mut self, id: u64) {
        self.start_task(id);
    }

    pub fn remove_task(&mut self, id: u64) {
        if let Some(t) = self.downloads.iter().find(|t| t.id == id) {
            t.cancel.store(true, Ordering::Relaxed);
            // Drop the partial file too — removing a task is an explicit
            // "forget this download" action, not a pause.
            let _ = std::fs::remove_file(updates_ui::part_path(&t.name, &t.url));
        }
        self.downloads.retain(|t| t.id != id);
        self.save_downloads();
        self.invalidate();
    }

    pub fn clear_finished(&mut self) {
        for t in &self.downloads {
            if !matches!(t.state, DlState::Queued | DlState::Downloading) {
                let _ = std::fs::remove_file(updates_ui::part_path(&t.name, &t.url));
            }
        }
        self.downloads
            .retain(|t| matches!(t.state, DlState::Queued | DlState::Downloading));
        self.status = "已清空下载记录".into();
        self.save_downloads();
        self.invalidate();
    }

    /// Newest task for an app — drives the card / detail button state.
    pub fn task_for(&self, name: &str) -> Option<&DlTask> {
        self.downloads.iter().rev().find(|t| t.name == name)
    }

    pub fn active_task(&self) -> Option<&DlTask> {
        self.downloads
            .iter()
            .rev()
            .find(|t| t.state == DlState::Downloading)
            .or_else(|| {
                self.downloads
                    .iter()
                    .rev()
                    .find(|t| t.state == DlState::Queued)
            })
    }

    pub fn active_count(&self) -> usize {
        self.downloads
            .iter()
            .filter(|t| matches!(t.state, DlState::Downloading | DlState::Queued))
            .count()
    }

    /// Called by the 500 ms window timer. Sampling on a fixed clock (rather
    /// than on each progress message) keeps the graph evenly spaced even when
    /// a fast connection emits dozens of updates per second.
    pub fn sample_speeds(&mut self) {
        let mut net = 0.0f64;
        let mut disk = 0.0f64;
        for t in &self.downloads {
            if t.state == DlState::Downloading {
                net += t.speed;
                disk += t.disk_speed;
            }
        }
        if self.speed_history.len() >= SPEED_SAMPLES {
            self.speed_history.pop_front();
        }
        self.speed_history.push_back(SpeedSample { net, disk });
    }

    pub fn dl_max_scroll(&self) -> i32 {
        let visible = (self.height as f32 - STATUS_H - DL_ROWS_TOP).max(0.0);
        let content = self.downloads.len() as f32 * (DL_ROW_H + GAP);
        (content - visible).max(0.0) as i32
    }

    // -----------------------------------------------------------------------
    // Download centre page
    // -----------------------------------------------------------------------

    pub fn dl_row_rect(&self, i: usize) -> Option<D2D_RECT_F> {
        let w = self.width as f32;
        let h = self.height as f32;
        let x0 = NAV_W + PAD;
        let x1 = w - PAD;
        // Three 80px actions + the icon/text band need ~450 px. MIN_W guarantees
        // 752, but keep the guard honest so no band can go negative.
        if x1 <= x0 + 460.0 {
            return None;
        }
        let top = DL_ROWS_TOP + i as f32 * (DL_ROW_H + GAP) - self.scroll as f32;
        if top + DL_ROW_H < DL_ROWS_TOP || top > h - STATUS_H {
            return None;
        }
        Some(gfx::rect(x0, top, x1, top + DL_ROW_H))
    }

    /// Three right-aligned slots: primary (安装/取消/重试), secondary
    /// (打开位置/移除) and tertiary (移除). Kept disjoint at every width so a
    /// finished row can offer "安装" and "打开位置" at the same time.
    pub fn dl_buttons(r: D2D_RECT_F) -> (D2D_RECT_F, D2D_RECT_F, D2D_RECT_F) {
        (
            gfx::rect(r.right - 96.0, r.top + 32.0, r.right - 16.0, r.top + 64.0),
            gfx::rect(r.right - 180.0, r.top + 32.0, r.right - 104.0, r.top + 64.0),
            gfx::rect(r.right - 264.0, r.top + 32.0, r.right - 188.0, r.top + 64.0),
        )
    }

    fn clear_rect(&self) -> D2D_RECT_F {
        let w = self.width as f32;
        gfx::rect(w - 158.0, TOP_H + 20.0, w - PAD, TOP_H + 52.0)
    }

    /// Steam-style transfer monitor: two translucent bar series (instantaneous
    /// network / disk throughput) plus two smoothed trend lines, over a 36 s
    /// rolling window sampled by the window timer.
    fn render_speed_panel(&mut self, w: f32) {
        let p = self.gfx.pal;
        let panel = gfx::rect(
            NAV_W + PAD,
            DL_CHART_TOP,
            w - PAD,
            DL_CHART_TOP + DL_CHART_H,
        );
        if panel.right <= panel.left + 320.0 {
            return;
        }
        self.gfx.round(panel, 14.0, p.card);
        self.gfx.outline(panel, p.border, 0.8);
        let l = panel.left + 16.0;
        let r = panel.right - 16.0;
        let t = panel.top;

        let hist: Vec<SpeedSample> = self.speed_history.iter().copied().collect();
        let mut net = 0.0f64;
        let mut disk = 0.0f64;
        let mut total_bytes = 0u64;
        for task in &self.downloads {
            total_bytes = total_bytes.saturating_add(task.done);
            if task.state == DlState::Downloading {
                net += task.speed;
                disk += task.disk_speed;
            }
        }
        let peak = hist
            .iter()
            .fold(net.max(disk), |m, s| m.max(s.net).max(s.disk));
        let scale = nice_scale(peak);
        let speed_text = |v: f64| {
            if v > 0.0 {
                fmt_speed(v)
            } else {
                "0 B/s".into()
            }
        };
        let net_text = speed_text(net);
        let disk_text = speed_text(disk);
        let peak_text = speed_text(peak);

        // Three proportional columns. Fixed offsets used to collide on a
        // 1000 DIP window: "累计/峰值" ran into the disk-speed label.
        let (x1, x2, x3) = speed_header_columns(l, r);

        self.gfx.text(
            "实时传输",
            gfx::rect(x1, t + 8.0, x2 - 8.0, t + 28.0),
            12,
            true,
            p.text,
        );
        self.gfx.text(
            &net_text,
            gfx::rect(x1, t + 26.0, x2 - 8.0, t + 54.0),
            21,
            true,
            p.accent,
        );
        self.gfx.text(
            "网络接收",
            gfx::rect(x1, t + 52.0, x2 - 8.0, t + 66.0),
            10,
            false,
            p.sub,
        );
        self.gfx.text(
            "写盘速度",
            gfx::rect(x2, t + 8.0, x3 - 8.0, t + 28.0),
            10,
            false,
            p.sub,
        );
        self.gfx.text(
            &disk_text,
            gfx::rect(x2, t + 26.0, x3 - 8.0, t + 52.0),
            16,
            true,
            p.disk_line,
        );
        self.gfx.text(
            &format!("累计 {}  ·  峰值 {}", fmt_size(total_bytes), peak_text),
            gfx::rect(x3, t + 8.0, r, t + 30.0),
            11,
            false,
            p.sub,
        );
        self.gfx
            .fill(gfx::rect(x3, t + 38.0, x3 + 8.0, t + 46.0), p.net_line);
        self.gfx.text(
            "网络",
            gfx::rect(x3 + 13.0, t + 32.0, x3 + 51.0, t + 54.0),
            10,
            false,
            p.sub,
        );
        self.gfx.fill(
            gfx::rect(x3 + 57.0, t + 38.0, x3 + 65.0, t + 46.0),
            p.disk_line,
        );
        self.gfx.text(
            "写盘",
            gfx::rect(x3 + 70.0, t + 32.0, x3 + 108.0, t + 54.0),
            10,
            false,
            p.sub,
        );
        self.gfx.text(
            "柱 = 瞬时  ·  线 = 趋势",
            gfx::rect(x3 + 114.0, t + 32.0, r, t + 54.0),
            10,
            false,
            p.sub,
        );

        let graph = gfx::rect(l, t + 70.0, r, t + 148.0);
        let gh = graph.bottom - graph.top;
        for i in 0..=3 {
            let y = graph.top + gh * i as f32 / 3.0;
            self.gfx
                .fill(gfx::rect(graph.left, y, graph.right, y + 0.6), p.border);
        }
        let slot = (graph.right - graph.left) / SPEED_SAMPLES as f32;
        let bar_w = (slot * 0.30).max(1.0);
        for (i, s) in hist.iter().enumerate() {
            let x = graph.left + i as f32 * slot;
            let hn = (s.net / scale).clamp(0.0, 1.0) as f32 * gh;
            let hd = (s.disk / scale).clamp(0.0, 1.0) as f32 * gh;
            if hn > 0.0 {
                self.gfx.fill(
                    gfx::rect(
                        x + slot * 0.10,
                        graph.bottom - hn,
                        x + slot * 0.10 + bar_w,
                        graph.bottom,
                    ),
                    p.net,
                );
            }
            if hd > 0.0 {
                self.gfx.fill(
                    gfx::rect(
                        x + slot * 0.55,
                        graph.bottom - hd,
                        x + slot * 0.55 + bar_w,
                        graph.bottom,
                    ),
                    p.disk,
                );
            }
        }
        // Smoothed trend lines over the same window.
        let smooth = |hist: &[SpeedSample], net_series: bool| -> Vec<f64> {
            let pick = |s: &SpeedSample| if net_series { s.net } else { s.disk };
            (0..hist.len())
                .map(|i| {
                    let a = pick(&hist[i.saturating_sub(1)]);
                    let b = pick(&hist[i]);
                    let c = pick(&hist[(i + 1).min(hist.len() - 1)]);
                    (a + 2.0 * b + c) / 4.0
                })
                .collect()
        };
        let y_of = |v: f64| graph.bottom - (v / scale).clamp(0.0, 1.0) as f32 * gh;
        if peak > 0.0 {
            for (series, color) in [(true, p.net_line), (false, p.disk_line)] {
                let values = smooth(&hist, series);
                for i in 1..values.len() {
                    let x0 = graph.left + (i as f32 - 0.5) * slot;
                    let x1 = graph.left + (i as f32 + 0.5) * slot;
                    self.gfx
                        .line(x0, y_of(values[i - 1]), x1, y_of(values[i]), color, 1.6);
                }
            }
        }
        self.gfx.text(
            &fmt_speed(scale),
            gfx::rect(
                graph.left + 4.0,
                graph.top + 2.0,
                graph.left + 96.0,
                graph.top + 18.0,
            ),
            10,
            false,
            p.sub,
        );
        // Axis labels sit on top of the newest bars; give them an opaque chip
        // so the numbers stay legible at full speed.
        self.gfx.fill(
            gfx::rect(
                graph.left + 2.0,
                graph.bottom - 17.0,
                graph.left + 28.0,
                graph.bottom - 1.0,
            ),
            p.card,
        );
        self.gfx.text(
            "0",
            gfx::rect(
                graph.left + 4.0,
                graph.bottom - 16.0,
                graph.left + 26.0,
                graph.bottom - 2.0,
            ),
            10,
            false,
            p.sub,
        );
        self.gfx.fill(
            gfx::rect(
                graph.right - 58.0,
                graph.bottom - 17.0,
                graph.right - 2.0,
                graph.bottom - 1.0,
            ),
            p.card,
        );
        self.gfx.text(
            "-36s",
            gfx::rect(
                graph.right - 56.0,
                graph.bottom - 16.0,
                graph.right - 4.0,
                graph.bottom - 2.0,
            ),
            10,
            false,
            p.sub,
        );
        if peak <= 0.0 {
            self.gfx.center_text(
                "开始下载后，这里会显示最近 36 秒的网络与写盘速度",
                gfx::rect(
                    graph.left,
                    graph.top + 18.0,
                    graph.right,
                    graph.bottom - 16.0,
                ),
                12,
                false,
                p.sub,
            );
        }
        self.gfx.text(
            "每 0.5 秒采样  ·  柱为瞬时速度（网络 / 写盘），折线为平滑趋势  ·  仅在本机统计",
            gfx::rect(l, t + 150.0, r, t + 166.0),
            10,
            false,
            p.sub,
        );
    }

    pub fn render_downloads(&mut self, w: f32, _h: f32) {
        let p = self.gfx.pal;
        let active = self.active_count();
        self.gfx.text(
            "下载管理",
            gfx::rect(NAV_W + PAD, TOP_H + 12.0, w - 200.0, TOP_H + 44.0),
            19,
            true,
            p.text,
        );
        let sub = if active > 0 {
            format!(
                "{} 个任务进行中  ·  共 {} 个任务  ·  文件保存在 Downloads",
                active,
                self.downloads.len()
            )
        } else {
            format!(
                "共 {} 个任务  ·  完成后点「安装」运行安装包，或「打开位置」查看文件",
                self.downloads.len()
            )
        };
        self.gfx.text(
            &sub,
            gfx::rect(NAV_W + PAD, TOP_H + 46.0, w - 200.0, TOP_H + 72.0),
            11,
            false,
            p.sub,
        );
        let cr = self.clear_rect();
        draw_button(
            &mut self.gfx,
            p,
            cr,
            "清空已完成",
            false,
            self.hover == Some((HIT_DL_CLEAR, 0)),
            self.downloads
                .iter()
                .all(|t| matches!(t.state, DlState::Queued | DlState::Downloading)),
        );
        self.render_speed_panel(w);

        if self.downloads.is_empty() {
            let top = DL_ROWS_TOP;
            self.gfx.round(
                gfx::rect(NAV_W + PAD, top, w - PAD, top + 108.0),
                14.0,
                p.accent_soft,
            );
            self.gfx.center_text(
                "还没有下载任务",
                gfx::rect(NAV_W + PAD, top + 14.0, w - PAD, top + 56.0),
                17,
                true,
                p.text,
            );
            self.gfx.center_text(
                "在应用卡片或详情页点「获取」，这里会实时显示进度、速度和剩余大小",
                gfx::rect(NAV_W + PAD, top + 56.0, w - PAD, top + 94.0),
                12,
                false,
                p.sub,
            );
            return;
        }

        for (i, t) in self.downloads.iter().enumerate() {
            let Some(r) = self.dl_row_rect(i) else {
                continue;
            };
            let hovered = self.hover.is_some_and(|(k, n)| n == i && is_dl_hit(k));
            self.gfx
                .round(r, 14.0, if hovered { p.hover } else { p.card });
            self.gfx
                .outline(r, p.border, if hovered { 1.2 } else { 0.8 });

            let icon = gfx::rect(r.left + 16.0, r.top + 22.0, r.left + 72.0, r.top + 78.0);
            let key = t.logo.as_deref().unwrap_or("");
            if !self.gfx.bitmap(key, icon) {
                self.gfx.round(icon, 12.0, p.accent_soft);
                let ch: String = t.name.chars().take(1).collect();
                self.gfx.center_text(&ch, icon, 22, true, p.accent);
            }

            let tx = r.left + 88.0;
            // Three button slots occupy the right 264 px; keep every text and
            // progress band strictly to their left (0.4.0 negative-width bug).
            let text_right = r.right - 276.0;
            self.gfx.text(
                &t.name,
                gfx::rect(tx, r.top + 14.0, text_right, r.top + 38.0),
                14,
                true,
                p.text,
            );

            let (label, color) = match t.state {
                DlState::Queued => ("等待下载…".to_string(), p.sub),
                DlState::Downloading => {
                    let mut parts = vec![fmt_size(t.done)];
                    if t.total > 0 {
                        parts.push(fmt_size(t.total));
                    }
                    let mut s = parts.join(" / ");
                    if !fmt_speed(t.speed).is_empty() {
                        s.push_str(&format!("  ·  网络 {}", fmt_speed(t.speed)));
                    }
                    if !fmt_speed(t.disk_speed).is_empty() {
                        s.push_str(&format!("  ·  写盘 {}", fmt_speed(t.disk_speed)));
                    }
                    (s, p.sub)
                }
                DlState::Done => (
                    format!("已完成  ·  {}", fmt_size(t.done.max(t.total))),
                    p.accent,
                ),
                DlState::Failed => (
                    format!("失败：{}", t.error.as_deref().unwrap_or("未知错误")),
                    p.danger,
                ),
                DlState::Paused => {
                    let mut s = format!("已暂停  ·  {}", fmt_size(t.done));
                    if t.total > 0 {
                        s.push_str(&format!(" / {}", fmt_size(t.total)));
                    }
                    s.push_str("  ·  进度已保存，可继续");
                    (s, p.sub)
                }
                DlState::Canceled => ("已取消下载".to_string(), p.sub),
            };
            self.gfx.text(
                &label,
                gfx::rect(tx, r.top + 42.0, text_right, r.top + 64.0),
                11,
                false,
                color,
            );

            let bar = gfx::rect(tx, r.top + 72.0, text_right, r.top + 80.0);
            let bar_color = if t.state == DlState::Failed {
                p.danger
            } else {
                p.accent
            };
            progress_bar(&mut self.gfx, p, bar, t.frac(), bar_color);
            // 0.4.0 used `r.right - 196` for the right edge while the left
            // edge was `text_right + 8` (= r.right - 192), producing a rect
            // with negative width and an invisible percentage.
            self.gfx.text(
                &t.percent(),
                gfx::rect(
                    text_right + 10.0,
                    r.top + 64.0,
                    text_right + 64.0,
                    r.top + 88.0,
                ),
                12,
                true,
                bar_color,
            );

            let (primary, secondary, tertiary) = Self::dl_buttons(r);
            match t.state {
                DlState::Downloading | DlState::Queued => {
                    draw_button(
                        &mut self.gfx,
                        p,
                        primary,
                        "暂停",
                        false,
                        self.hover == Some((HIT_DL_PAUSE, i)),
                        false,
                    );
                    draw_button(
                        &mut self.gfx,
                        p,
                        secondary,
                        "取消",
                        false,
                        self.hover == Some((HIT_DL_CANCEL, i)),
                        false,
                    );
                }
                DlState::Paused => {
                    draw_button(
                        &mut self.gfx,
                        p,
                        primary,
                        "继续",
                        true,
                        self.hover == Some((HIT_DL_RETRY, i)),
                        false,
                    );
                    draw_button(
                        &mut self.gfx,
                        p,
                        tertiary,
                        "移除",
                        false,
                        self.hover == Some((HIT_DL_REMOVE, i)),
                        false,
                    );
                }
                DlState::Done => {
                    draw_button(
                        &mut self.gfx,
                        p,
                        primary,
                        "安装",
                        true,
                        self.hover == Some((HIT_DL_INSTALL, i)),
                        false,
                    );
                    draw_button(
                        &mut self.gfx,
                        p,
                        secondary,
                        "打开位置",
                        false,
                        self.hover == Some((HIT_DL_OPEN, i)),
                        false,
                    );
                    draw_button(
                        &mut self.gfx,
                        p,
                        tertiary,
                        "移除",
                        false,
                        self.hover == Some((HIT_DL_REMOVE, i)),
                        false,
                    );
                }
                DlState::Failed | DlState::Canceled => {
                    draw_button(
                        &mut self.gfx,
                        p,
                        primary,
                        "重试",
                        true,
                        self.hover == Some((HIT_DL_RETRY, i)),
                        false,
                    );
                    draw_button(
                        &mut self.gfx,
                        p,
                        tertiary,
                        "移除",
                        false,
                        self.hover == Some((HIT_DL_REMOVE, i)),
                        false,
                    );
                }
            }
        }
    }

    /// Compact live progress shown in the bottom bar on every page.
    pub fn render_status_progress(&mut self, w: f32, h: f32) {
        let p = self.gfx.pal;
        let Some((name, pct, speed, disk, frac)) = self.active_task().map(|t| {
            (
                t.name.clone(),
                t.percent(),
                fmt_speed(t.speed),
                fmt_speed(t.disk_speed),
                t.frac(),
            )
        }) else {
            return;
        };
        let x0 = (w - 400.0).max(200.0);
        let x1 = w - 196.0;
        if x1 <= x0 + 120.0 {
            return;
        }
        let caption = if disk.is_empty() {
            format!("{}  {}  {}", name, pct, speed)
        } else {
            format!("{}  {}  网 {}  盘 {}", name, pct, speed, disk)
        };
        self.gfx.text(
            &caption,
            gfx::rect(x0, h - STATUS_H + 2.0, x1, h - STATUS_H + 18.0),
            10,
            false,
            p.sub,
        );
        progress_bar(
            &mut self.gfx,
            p,
            gfx::rect(x0, h - STATUS_H + 23.0, x1, h - STATUS_H + 29.0),
            frac,
            p.accent,
        );
    }
}

fn is_dl_hit(k: u8) -> bool {
    matches!(
        k,
        HIT_DL_CANCEL | HIT_DL_RETRY | HIT_DL_INSTALL | HIT_DL_OPEN | HIT_DL_REMOVE | HIT_DL_PAUSE
    )
}

pub(crate) fn hit_targets(st: &AppState, x: f32, y: f32) -> Option<(u8, usize)> {
    if in_rect(x, y, st.clear_rect()) {
        return Some((HIT_DL_CLEAR, 0));
    }
    for i in 0..st.downloads.len() {
        let Some(r) = st.dl_row_rect(i) else {
            continue;
        };
        let (primary, secondary, tertiary) = AppState::dl_buttons(r);
        let t = &st.downloads[i];
        if in_rect(x, y, primary) {
            let kind = match t.state {
                DlState::Downloading | DlState::Queued => HIT_DL_PAUSE,
                DlState::Done => HIT_DL_INSTALL,
                _ => HIT_DL_RETRY,
            };
            return Some((kind, i));
        }
        // A running row puts "取消" (throw the partial bytes away) in the
        // second slot, right next to "暂停" (keep them for later).
        if in_rect(x, y, secondary) && matches!(t.state, DlState::Downloading | DlState::Queued) {
            return Some((HIT_DL_CANCEL, i));
        }
        // "打开位置" only exists on a finished row; "移除" lives in the
        // leftmost slot so it stays put across Done and Failed states.
        if in_rect(x, y, secondary) && t.state == DlState::Done {
            return Some((HIT_DL_OPEN, i));
        }
        if in_rect(x, y, tertiary)
            && matches!(
                t.state,
                DlState::Done | DlState::Failed | DlState::Canceled | DlState::Paused
            )
        {
            return Some((HIT_DL_REMOVE, i));
        }
    }
    None
}

pub(crate) fn apply(st: &mut AppState, kind: u8, i: usize) {
    let Some(id) = st.downloads.get(i).map(|t| t.id) else {
        return;
    };
    let path = st.downloads.get(i).and_then(|t| t.path.clone());
    match kind {
        HIT_DL_CANCEL => st.cancel_task(id),
        HIT_DL_PAUSE => st.pause_task(id),
        HIT_DL_RETRY => st.retry_task(id),
        HIT_DL_REMOVE => st.remove_task(id),
        HIT_DL_INSTALL => st.install_task(id),
        HIT_DL_OPEN => {
            if let Some(p) = path {
                open_folder(&p);
                st.status = "已在资源管理器中选中安装包".into();
                st.invalidate();
            }
        }
        HIT_DL_CLEAR => st.clear_finished(),
        _ => {}
    }
}

/// Status bar hit: jumping straight to the download centre.
pub(crate) fn status_hit(st: &AppState, x: f32, y: f32) -> bool {
    let w = st.width as f32;
    let h = st.height as f32;
    st.active_task().is_some()
        && y >= h - STATUS_H
        && y <= h
        && x >= (w - 400.0).max(200.0)
        && x <= w - 196.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(state: DlState, done: u64, total: u64) -> DlTask {
        DlTask {
            id: 1,
            name: "Test".into(),
            url: String::new(),
            logo: None,
            state,
            done,
            total,
            speed: 0.0,
            disk_speed: 0.0,
            path: None,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
            failover: false,
        }
    }

    #[test]
    fn legacy_error_snapshots_are_rewritten_on_restore() {
        let legacy =
            "服务器返回 HTTP 404（{\"errorcode\":-46628,\"errormsg\":\"file not found\"}）";
        let clean = sanitize_error(legacy);
        assert_eq!(clean, "下载地址已失效（HTTP 404）");
        // Already-readable and non-HTTP failures must survive verbatim.
        let short = "下载地址已失效（HTTP 404）";
        assert_eq!(sanitize_error(short), short);
        assert_eq!(sanitize_error("磁盘空间不足"), "磁盘空间不足");
        assert_eq!(
            sanitize_error("发送请求失败：0x80072EE7"),
            "无法解析下载服务器域名（0x80072EE7）"
        );
        assert_eq!(
            sanitize_error("服务器返回 HTTP 但状态码缺失"),
            "服务器返回 HTTP 但状态码缺失"
        );
    }

    #[test]
    fn only_dead_third_party_links_trigger_a_failover() {
        let dead = "服务器返回 HTTP 404（Err-Code: -46628）";
        assert!(link_retryable(false, "https://webcdn.m.qq.com/x.exe", dead));
        // DNS / connection failures are just as dead as a 404 and must fan out
        // to another source; the Tencent CDN mostly fails this way.
        assert!(link_retryable(
            false,
            "https://webcdn.m.qq.com/x.exe",
            "无法解析下载服务器域名（0x80072EE7）"
        ));
        assert!(link_retryable(
            false,
            "https://webcdn.m.qq.com/x.exe",
            "无法连接下载服务器（0x80072EFD）"
        ));
        assert!(link_retryable(
            false,
            "https://webcdn.m.qq.com/x.exe",
            "连接下载服务器超时（0x80072EE2）"
        ));
        // Disk / user-driven failures stay put.
        assert!(!link_retryable(
            false,
            "https://webcdn.m.qq.com/x.exe",
            "写入磁盘失败：磁盘空间不足"
        ));
        assert!(
            !link_retryable(true, "https://webcdn.m.qq.com/x.exe", dead),
            "a second failure must be final"
        );

        assert!(
            !link_retryable(
                false,
                "https://pc-store.lenovomm.cn/dlservice/download?softid=L1",
                dead
            ),
            "Lenovo links are generated per request"
        );
        assert!(!link_retryable(
            false,
            "https://webcdn.m.qq.com/x.exe",
            "已暂停"
        ));
    }

    #[test]
    fn sizes_and_speeds_are_human_readable() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(1536), "1.5 KB");
        assert_eq!(fmt_size(1024 * 1024), "1.0 MB");
        assert_eq!(fmt_speed(2.0 * 1024.0 * 1024.0), "2.0 MB/s");
        assert_eq!(fmt_speed(0.0), "");
    }

    /// Regression: the old downloader posted once per 8 MB, so a 1.1 MB
    /// installer produced *zero* progress updates. Now every percent emits.
    #[test]
    fn small_downloads_still_emit_many_progress_updates() {
        let mut p = Progress::new(Some(7), 0, "MiniApp");
        let total = 1_136_096u64;
        p.set_total(total);
        let before = p.posts;
        let mut sent = 0u64;
        for _ in 0..110 {
            p.add(10_000);
            sent += 10_000;
        }
        assert!(sent < total, "test models a small, incomplete download");
        assert!(
            p.posts - before >= 50,
            "a 1.1 MB download must produce many visible updates, got {}",
            p.posts - before
        );
        assert_eq!(p.total(), total);
    }

    #[test]
    fn task_fraction_and_percent_track_state() {
        assert_eq!(task(DlState::Done, 0, 100).frac(), 1.0);
        assert!((task(DlState::Downloading, 25, 100).frac() - 0.25).abs() < 1e-6);
        assert_eq!(task(DlState::Downloading, 25, 100).percent(), "25%");
        assert_eq!(task(DlState::Downloading, 1, 0).percent(), "…");
        assert_eq!(task(DlState::Failed, 50, 100).percent(), "50%");
    }

    #[test]
    fn speed_scale_uses_binary_steps_matching_fmt_size() {
        let k = 1024.0f64;
        assert_eq!(nice_scale(0.0), 64.0 * k);
        assert_eq!(nice_scale(100.0 * k), 128.0 * k);
        assert_eq!(nice_scale(3.0 * k * k), 4.0 * k * k);
        assert_eq!(nice_scale(8.0 * k * k), 8.0 * k * k);
        assert_eq!(nice_scale(1.7 * k * k), 2.0 * k * k);
        assert!(nice_scale(1.7 * k * k) >= 1.7 * k * k);
    }

    #[test]
    fn speed_graph_never_overlaps_the_task_rows() {
        assert!(DL_ROWS_TOP + DL_ROW_H < crate::app::MIN_H as f32 - STATUS_H);
        assert_eq!(SPEED_SAMPLES, 72, "72 samples x 500 ms = 36 s window");
    }

    #[test]
    fn speed_header_columns_stay_disjoint_at_every_window_width() {
        // Panel = [NAV_W + PAD + 16, w - PAD - 16] for the supported widths.
        for w in [1000.0f32, 1100.0, 1600.0, 2048.0, 2560.0] {
            let (l, r) = (NAV_W + PAD + 16.0, w - PAD - 16.0);
            let (x1, x2, x3) = speed_header_columns(l, r);
            assert!(x1 < x2 && x2 < x3 && x3 < r, "columns collide at w={w}");
            assert!(r - x3 >= 180.0 || x3 >= x2 + 8.0, "right column too narrow");
            // Legend needs 114 + label width inside the right column.
            assert!(r - x3 >= 114.0, "legend overflows at w={w}");
        }
    }

    #[test]
    fn disk_throughput_is_sampled_independently_of_network() {
        let mut p = Progress::new(Some(3), 0, "App");
        p.set_total(1 << 20);
        for _ in 0..10 {
            p.written(10_000);
            p.add(10_000);
        }
        // Written bytes are tracked separately so a slow disk can lag.
        assert_eq!(p.written, p.done);
        assert!(p.disk_speed >= 0.0);
    }

    /// A finished row must fit three disjoint actions (安装 / 打开位置 /
    /// 移除) and the progress percentage, with real (positive) widths.
    #[test]
    fn download_row_has_three_disjoint_actions_at_every_width() {
        for w in [1000.0f32, 1100.0, 1600.0, 2560.0] {
            let r = gfx::rect(NAV_W + PAD, 0.0, w - PAD, DL_ROW_H);
            let (primary, secondary, tertiary) = AppState::dl_buttons(r);
            assert!(
                secondary.right < primary.left,
                "w={w}: 打开位置 crosses 安装"
            );
            assert!(
                tertiary.right < secondary.left,
                "w={w}: 移除 crosses 打开位置"
            );
            for b in [primary, secondary, tertiary] {
                assert!(b.right - b.left >= 56.0, "w={w}: action button too narrow");
                assert!(b.right <= r.right - 16.0);
            }
            // Text and progress stop 12px before the leftmost button.
            let text_right = r.right - 276.0;
            assert!(text_right < tertiary.left, "w={w}: text crosses actions");
            assert!(
                text_right > r.left + 88.0 + 100.0,
                "w={w}: text band too narrow"
            );
            let pct = gfx::rect(text_right + 10.0, 0.0, text_right + 64.0, 24.0);
            assert!(pct.right > pct.left, "w={w}: percentage rect inverted");
        }
    }

    #[test]
    fn missing_installer_path_is_reported_not_launched() {
        // A path that cannot exist must fail before any shell call.
        let err = run_installer("C:\\__mindosshop_no_such_installer__.exe").unwrap_err();
        assert!(err.contains("不存在"), "unexpected message: {err}");
    }

    #[test]
    fn shell_error_codes_are_actionable() {
        assert!(shell_error(2).contains("找不到"));
        assert!(shell_error(5).contains("拒绝"));
        assert!(shell_error(11).contains("格式"));
        assert!(shell_error(99).contains("99"));
    }
}
