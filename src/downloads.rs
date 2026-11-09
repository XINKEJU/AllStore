//! Download centre: a real task queue with live progress, speed, cancel,
//! retry and "open containing folder". Every visible surface (app cards,
//! detail page, status bar, this page) reads the same task list.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

use crate::app::{
    AppState, GAP, HIT_DL_CANCEL, HIT_DL_CLEAR, HIT_DL_OPEN, HIT_DL_REMOVE, HIT_DL_RETRY, NAV_W,
    PAD, Payload, STATUS_H, TOP_H, WM_APP_DL_DONE, WM_APP_DL_PROGRESS, draw_button, in_rect, post,
};
use crate::gfx::{self, Color, Gfx, Palette};
use crate::updates_ui;

pub(crate) const DL_ROW_H: f32 = 100.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DlState {
    Queued,
    Downloading,
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
    /// Smoothed bytes per second.
    pub speed: f64,
    pub path: Option<String>,
    pub error: Option<String>,
    pub cancel: Arc<AtomicBool>,
}

impl DlTask {
    pub fn frac(&self) -> f32 {
        match self.state {
            DlState::Done => 1.0,
            DlState::Downloading | DlState::Queued if self.total == 0 => 0.10,
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
            tick: Instant::now(),
            last_done: 0,
            last_pct: -1,
            posts: 0,
        }
    }

    pub fn set_total(&mut self, total: u64) {
        self.total = total;
        self.emit(true);
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
            let instant = if elapsed > 0.0 {
                delta as f64 / elapsed
            } else {
                0.0
            };
            self.speed = if self.speed == 0.0 {
                instant
            } else {
                self.speed * 0.65 + instant * 0.35
            };
            self.tick = now;
            self.last_done = self.done;
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
    // Selects the file in Explorer; never executes the installer itself.
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path))
        .spawn();
}

impl AppState {
    /// Reuse a live task instead of downloading the same app twice.
    pub fn new_download(&mut self, name: &str, url: &str, logo: Option<String>) -> u64 {
        if let Some(t) =
            self.downloads.iter().rev().find(|t| {
                t.name == name && matches!(t.state, DlState::Queued | DlState::Downloading)
            })
        {
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
            path: None,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        id
    }

    /// Queue a batch before one worker drains it serially (used by
    /// "update all"). Jobs stay "waiting" until their turn actually starts.
    pub fn mark_running(&mut self, ids: &[u64]) {
        for id in ids {
            if let Some(t) = self.downloads.iter_mut().find(|t| t.id == *id) {
                t.state = DlState::Queued;
                t.done = 0;
                t.total = 0;
                t.speed = 0.0;
                t.error = None;
                t.cancel.store(false, Ordering::Relaxed);
            }
        }
    }

    pub fn start_task(&mut self, id: u64) {
        let Some(t) = self.downloads.iter_mut().find(|t| t.id == id) else {
            return;
        };
        if matches!(t.state, DlState::Downloading | DlState::Queued) && t.done > 0 {
            return;
        }
        t.state = DlState::Downloading;
        t.done = 0;
        t.total = 0;
        t.speed = 0.0;
        t.error = None;
        t.cancel.store(false, Ordering::Relaxed);
        let (name, url, cancel) = (t.name.clone(), t.url.clone(), t.cancel.clone());
        let h = self.hwnd_usize();
        self.status = format!("开始下载 {} …", name);
        self.invalidate();
        std::thread::spawn(move || {
            let result = updates_ui::download_package_ex(&name, &url, h, Some(id), Some(cancel));
            post(h, WM_APP_DL_DONE, Payload::DlDone { id, result });
        });
    }

    pub fn on_dl_progress(&mut self, id: u64, done: u64, total: u64, speed: f64) {
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
        self.invalidate();
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
                t.speed = 0.0;
                let short = path.rsplit(['\\', '/']).next().unwrap_or(&path);
                self.status = format!("✓ {} 已下载到 Downloads（{}）", t.name, short);
            }
            Err(e) => {
                t.state = if t.cancel.load(Ordering::Relaxed) {
                    DlState::Canceled
                } else {
                    DlState::Failed
                };
                t.error = Some(e.clone());
                t.speed = 0.0;
                self.status = format!("{} 下载未完成：{}", t.name, e);
            }
        }
        self.invalidate();
    }

    pub fn cancel_task(&mut self, id: u64) {
        if let Some(t) = self.downloads.iter().find(|t| t.id == id) {
            t.cancel.store(true, Ordering::Relaxed);
            self.status = format!("正在取消 {} 的下载…", t.name);
            self.invalidate();
        }
    }

    pub fn retry_task(&mut self, id: u64) {
        self.start_task(id);
    }

    pub fn remove_task(&mut self, id: u64) {
        if let Some(t) = self.downloads.iter().find(|t| t.id == id) {
            t.cancel.store(true, Ordering::Relaxed);
        }
        self.downloads.retain(|t| t.id != id);
        self.invalidate();
    }

    pub fn clear_finished(&mut self) {
        self.downloads
            .retain(|t| matches!(t.state, DlState::Queued | DlState::Downloading));
        self.status = "已清空下载记录".into();
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

    pub fn dl_max_scroll(&self) -> i32 {
        let visible = (self.height as f32 - TOP_H - STATUS_H - 118.0).max(0.0);
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
        if x1 <= x0 + 320.0 {
            return None;
        }
        let top = TOP_H + 110.0 + i as f32 * (DL_ROW_H + GAP) - self.scroll as f32;
        if top + DL_ROW_H < TOP_H + 96.0 || top > h - STATUS_H {
            return None;
        }
        Some(gfx::rect(x0, top, x1, top + DL_ROW_H))
    }

    pub fn dl_buttons(r: D2D_RECT_F) -> (D2D_RECT_F, D2D_RECT_F) {
        (
            gfx::rect(r.right - 100.0, r.top + 32.0, r.right - 16.0, r.top + 64.0),
            gfx::rect(r.right - 186.0, r.top + 32.0, r.right - 108.0, r.top + 64.0),
        )
    }

    fn clear_rect(&self) -> D2D_RECT_F {
        let w = self.width as f32;
        gfx::rect(w - 158.0, TOP_H + 20.0, w - PAD, TOP_H + 52.0)
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
                "共 {} 个任务  ·  已下载的安装包保存在 Downloads，需手动运行安装",
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

        if self.downloads.is_empty() {
            self.gfx.round(
                gfx::rect(NAV_W + PAD, TOP_H + 110.0, w - PAD, TOP_H + 218.0),
                14.0,
                p.accent_soft,
            );
            self.gfx.center_text(
                "还没有下载任务",
                gfx::rect(NAV_W + PAD, TOP_H + 124.0, w - PAD, TOP_H + 166.0),
                17,
                true,
                p.text,
            );
            self.gfx.center_text(
                "在应用卡片或详情页点「获取」，这里会实时显示进度、速度和剩余大小",
                gfx::rect(NAV_W + PAD, TOP_H + 166.0, w - PAD, TOP_H + 204.0),
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
            let text_right = r.right - 200.0;
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
                        s.push_str(&format!("  ·  {}", fmt_speed(t.speed)));
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
                    r.right - 16.0,
                    r.top + 88.0,
                ),
                12,
                true,
                bar_color,
            );

            let (primary, secondary) = Self::dl_buttons(r);
            match t.state {
                DlState::Downloading | DlState::Queued => {
                    draw_button(
                        &mut self.gfx,
                        p,
                        primary,
                        "取消",
                        false,
                        self.hover == Some((HIT_DL_CANCEL, i)),
                        false,
                    );
                }
                DlState::Done => {
                    draw_button(
                        &mut self.gfx,
                        p,
                        primary,
                        "打开位置",
                        true,
                        self.hover == Some((HIT_DL_OPEN, i)),
                        false,
                    );
                    draw_button(
                        &mut self.gfx,
                        p,
                        secondary,
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
                        secondary,
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
        let Some((name, pct, speed, frac)) = self
            .active_task()
            .map(|t| (t.name.clone(), t.percent(), fmt_speed(t.speed), t.frac()))
        else {
            return;
        };
        let x0 = (w - 400.0).max(200.0);
        let x1 = w - 196.0;
        if x1 <= x0 + 120.0 {
            return;
        }
        let caption = format!("{}  {}  {}", name, pct, speed);
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
        HIT_DL_CANCEL | HIT_DL_RETRY | HIT_DL_OPEN | HIT_DL_REMOVE
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
        let (primary, secondary) = AppState::dl_buttons(r);
        let t = &st.downloads[i];
        if in_rect(x, y, primary) {
            let kind = match t.state {
                DlState::Downloading | DlState::Queued => HIT_DL_CANCEL,
                DlState::Done => HIT_DL_OPEN,
                _ => HIT_DL_RETRY,
            };
            return Some((kind, i));
        }
        if in_rect(x, y, secondary)
            && matches!(t.state, DlState::Done | DlState::Failed | DlState::Canceled)
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
        HIT_DL_RETRY => st.retry_task(id),
        HIT_DL_REMOVE => st.remove_task(id),
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
            path: None,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
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
}
