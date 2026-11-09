//! Window state, message loop handling, layout and interaction.
//!
//! Layout uses a single set of spacing tokens and one shared hit-test so the
//! rendering and the click targets can never drift apart.
use std::collections::HashSet;
use std::ffi::c_void;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Gdi::{InvalidateRect, ValidateRect};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, Gfx, Palette};
use crate::model::{App, ClassGroup, Detail};
use crate::net;
use crate::update::{self, UpdateItem};
use crate::updates_ui;

pub const WM_APP_DATA: u32 = 0x8000 + 1;
pub const WM_APP_LIST: u32 = 0x8000 + 2;
pub const WM_APP_ICON: u32 = 0x8000 + 3;
pub const WM_APP_DETAIL: u32 = 0x8000 + 4;
pub const WM_APP_STATUS: u32 = 0x8000 + 5;
pub const WM_APP_DONE: u32 = 0x8000 + 6;
pub const WM_APP_UPDATE_PROGRESS: u32 = 0x8000 + 7;
pub const WM_APP_UPDATE_DONE: u32 = 0x8000 + 8;

// Design tokens: one grid for every screen.
pub(crate) const NAV_W: f32 = 200.0;
pub(crate) const TOP_H: f32 = 68.0;
pub(crate) const STATUS_H: f32 = 32.0;
pub(crate) const PAD: f32 = 24.0;
pub(crate) const GAP: f32 = 14.0;
pub(crate) const CARD_W: f32 = 272.0;
pub(crate) const CARD_H: f32 = 128.0;
pub(crate) const BTN_H: f32 = 32.0;
pub(crate) const NAV_ROW_H: f32 = 40.0;
pub(crate) const ROW_H: f32 = 84.0;
pub(crate) const MIN_W: i32 = 940;
pub(crate) const MIN_H: i32 = 580;

pub const HIT_SEARCH_BOX: u8 = 0;
pub const HIT_SEARCH_BTN: u8 = 1;
pub const HIT_NAV: u8 = 2;
pub const HIT_BACK: u8 = 3;
pub const HIT_DOWNLOAD: u8 = 4;
pub const HIT_CHECK: u8 = 5;
pub const HIT_ALL: u8 = 6;
pub const HIT_ROW_UPDATE: u8 = 7;
pub const HIT_ROW_IGNORE: u8 = 8;
pub const HIT_ROW: u8 = 9;
pub const HIT_INSTALL: u8 = 10;
pub const HIT_CARD: u8 = 11;

pub enum Payload {
    Data(Result<(String, Vec<ClassGroup>, Vec<App>), String>),
    /// (result, epoch) — stale responses from rapid navigation are discarded.
    List(Result<(Vec<App>, String), String>, u64),
    Icon {
        key: String,
        bytes: Vec<u8>,
    },
    Detail(Result<Detail, String>, u64),
    Status(String),
    /// (download result, is_update)
    Done(Result<String, String>, bool),
    UpdateProgress(usize, usize),
    UpdateDone(Result<Vec<UpdateItem>, String>),
}

pub(crate) fn post(hwnd_usize: usize, msg: u32, p: Payload) {
    unsafe {
        let h = HWND(hwnd_usize as *mut c_void);
        let raw = Box::into_raw(Box::new(p)) as isize;
        if PostMessageW(Some(h), msg, WPARAM(0), LPARAM(raw)).is_err() {
            drop(Box::from_raw(raw as *mut Payload));
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum NavKind {
    Browse,
    Updates,
}

#[derive(Clone)]
pub struct NavItem {
    pub label: String,
    pub code: String,
    pub id: String,
    pub kind: NavKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browse,
    Detail,
    Updates,
}

pub struct AppState {
    pub hwnd: HWND,
    pub gfx: Gfx,
    pub width: u32,
    pub height: u32,
    pub token: String,
    pub nav: Vec<NavItem>,
    pub active_nav: usize,
    pub apps: Vec<App>,
    pub list_title: String,
    pub search: Vec<u16>,
    pub focus_search: bool,
    pub mode: Mode,
    pub detail_index: usize,
    pub detail_extra: Option<Detail>,
    pub scroll: i32,
    /// (hit kind, index) of the control under the pointer.
    pub hover: Option<(u8, usize)>,
    pub status: String,
    pub loading: bool,
    pub updates: Vec<UpdateItem>,
    pub update_checking: bool,
    pub update_progress: (usize, usize),
    pub update_checked_at: u64,
    pub ignored: Vec<String>,
    list_epoch: u64,
    detail_epoch: u64,
    pending_icons: HashSet<String>,
}

// ---------------------------------------------------------------------------
// Text helpers. Direct2D's DrawText wraps on its own; pre-wrapping with a
// slightly narrower width keeps line breaks predictable and lets us add a
// proper ellipsis instead of clipping a word in half.
// ---------------------------------------------------------------------------

fn char_width(c: char, size: u32) -> f32 {
    let s = size as f32;
    if (c as u32) < 0x2E80 {
        s * 0.56
    } else {
        s * 1.02
    }
}

pub(crate) fn wrap_text(s: &str, width: f32, size: u32) -> String {
    let clean = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<String> = Vec::new();
    for para in clean.split('\n') {
        let chars: Vec<char> = para.chars().filter(|c| !c.is_control()).collect();
        if chars.is_empty() {
            if !out.is_empty() && !out.last().is_some_and(|l| l.is_empty()) {
                out.push(String::new());
            }
            continue;
        }
        let mut start = 0usize;
        while start < chars.len() {
            let mut used = 0.0f32;
            let mut end = start;
            while end < chars.len() {
                let cw = char_width(chars[end], size);
                if used + cw > width && end > start {
                    break;
                }
                used += cw;
                end += 1;
            }
            if end == start {
                end = start + 1;
            }
            out.push(
                chars[start..end]
                    .iter()
                    .collect::<String>()
                    .trim_end()
                    .to_string(),
            );
            start = end;
        }
    }
    out.join("\n")
}

pub(crate) fn clip_lines(s: &str, width: f32, size: u32, max_lines: usize) -> String {
    let wrapped = wrap_text(s, width, size);
    let lines: Vec<&str> = wrapped.lines().collect();
    if lines.len() <= max_lines {
        return wrapped;
    }
    let mut out: Vec<String> = lines[..max_lines.saturating_sub(1)]
        .iter()
        .map(|l| l.to_string())
        .collect();
    let mut last: String = lines[max_lines - 1].chars().take(64).collect();
    last.pop(); // leave room for the ellipsis
    last.push('…');
    out.push(last);
    out.join("\n")
}

pub(crate) fn ellipsize(s: &str, width: f32, size: u32) -> String {
    let chars: Vec<char> = s.chars().filter(|c| !c.is_control()).collect();
    let mut used = 0.0f32;
    let mut n = 0usize;
    for c in &chars {
        let cw = char_width(*c, size);
        if used + cw > width {
            break;
        }
        used += cw;
        n += 1;
    }
    if n >= chars.len() {
        return chars.iter().collect();
    }
    let mut out: String = chars[..n.max(1)].iter().collect();
    out.pop();
    out.push('…');
    out
}

fn estimate_width(s: &str, size: u32) -> f32 {
    s.chars().map(|c| char_width(c, size)).sum()
}

fn short_path(s: &str) -> String {
    s.rsplit(['\\', '/']).next().unwrap_or(s).to_string()
}

pub(crate) fn draw_button(
    g: &mut Gfx,
    p: Palette,
    r: D2D_RECT_F,
    label: &str,
    primary: bool,
    hovered: bool,
    disabled: bool,
) {
    let (bg, fg) = if disabled {
        (p.subtle, p.sub)
    } else if primary {
        (p.accent, p.on_accent)
    } else {
        (p.card, p.text)
    };
    g.round(r, 8.0, bg);
    if !primary && !disabled {
        g.outline(r, p.border, 1.0);
    }
    if hovered && !disabled {
        g.outline(r, if primary { p.on_accent } else { p.accent }, 1.5);
    }
    g.center_text(label, r, 13, true, fg);
}

// ---------------------------------------------------------------------------

struct DetailLayout {
    intro: String,
    intro_h: f32,
    whats_new: String,
    whats_new_h: f32,
    info: Vec<(String, String)>,
    total_h: f32,
}

impl AppState {
    pub fn new(hwnd: HWND, gfx: Gfx, width: u32, height: u32) -> Self {
        let cache = update::load_cache();
        Self {
            hwnd,
            gfx,
            width,
            height,
            token: String::new(),
            nav: vec![
                NavItem {
                    label: "推荐".into(),
                    code: "soft".into(),
                    id: "-1".into(),
                    kind: NavKind::Browse,
                },
                NavItem {
                    label: "软件更新".into(),
                    code: String::new(),
                    id: String::new(),
                    kind: NavKind::Updates,
                },
            ],
            active_nav: 0,
            apps: Vec::new(),
            list_title: "推荐应用".into(),
            search: Vec::new(),
            focus_search: false,
            mode: Mode::Browse,
            detail_index: 0,
            detail_extra: None,
            scroll: 0,
            hover: None,
            status: "正在连接应用目录…".into(),
            loading: true,
            updates: cache.items,
            update_checking: false,
            update_progress: (0, 0),
            update_checked_at: cache.checked_at,
            ignored: cache.ignored,
            list_epoch: 0,
            detail_epoch: 0,
            pending_icons: HashSet::new(),
        }
    }

    pub(crate) fn hwnd_usize(&self) -> usize {
        self.hwnd.0 as usize
    }

    fn search_text(&self) -> String {
        String::from_utf16_lossy(&self.search)
    }

    pub fn queue_icons(&mut self) {
        let mut urls: Vec<String> = self
            .apps
            .iter()
            .filter_map(|a| a.logo_url().map(str::to_string))
            .collect();
        urls.extend(self.updates.iter().filter_map(|u| u.logo.clone()));
        let mut queued = 0;
        for url in urls {
            if self.gfx.icons.contains_key(&url) || !self.pending_icons.insert(url.clone()) {
                continue;
            }
            let h = self.hwnd_usize();
            std::thread::spawn(move || {
                // An empty result still posts, so a failed icon can be retried.
                let bytes = net::fetch_binary(&url).unwrap_or_default();
                post(h, WM_APP_ICON, Payload::Icon { key: url, bytes });
            });
            queued += 1;
            if queued >= 28 {
                break;
            }
        }
    }

    fn set_apps(&mut self, apps: Vec<App>, title: &str) {
        self.apps = apps;
        self.list_title = title.to_string();
        self.scroll = 0;
        self.mode = Mode::Browse;
        self.loading = false;
        self.status = format!(
            "{}  ·  共 {} 款应用  ·  纯净模式（无运营位 / 无遥测）",
            title,
            self.apps.len()
        );
        self.queue_icons();
        self.invalidate();
    }

    pub(crate) fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn load_nav(&mut self, classes: &[ClassGroup]) {
        let was_updates = self
            .nav
            .get(self.active_nav)
            .is_some_and(|n| n.kind == NavKind::Updates);
        self.nav.clear();
        self.nav.push(NavItem {
            label: "推荐".into(),
            code: "soft".into(),
            id: "-1".into(),
            kind: NavKind::Browse,
        });
        self.nav.push(NavItem {
            label: "软件更新".into(),
            code: String::new(),
            id: String::new(),
            kind: NavKind::Updates,
        });
        for g in classes {
            if g.code != "soft" {
                continue;
            }
            for c in g.list.iter().take(12) {
                if c.name == "全部" {
                    continue;
                }
                self.nav.push(NavItem {
                    label: c.name.clone(),
                    code: g.code.clone(),
                    id: c.id.clone(),
                    kind: NavKind::Browse,
                });
            }
        }
        self.active_nav = if was_updates { 1 } else { 0 };
    }

    fn next_epoch(&mut self) -> u64 {
        self.list_epoch += 1;
        self.list_epoch
    }

    fn select_nav(&mut self, idx: usize) {
        if idx >= self.nav.len() {
            return;
        }
        let item = self.nav[idx].clone();
        self.active_nav = idx;
        self.focus_search = false;
        self.mode = Mode::Browse;
        self.scroll = 0;
        if item.kind == NavKind::Updates {
            self.mode = Mode::Updates;
            self.show_updates();
            return;
        }
        let epoch = self.next_epoch();
        self.status = format!("正在加载「{}」…", item.label);
        self.loading = true;
        self.invalidate();
        let h = self.hwnd_usize();
        let token = self.token.clone();
        let label = item.label.clone();
        std::thread::spawn(move || {
            let r = net::class_apps(&token, &item.code, &item.id, 36).map(|v| (v, label));
            post(h, WM_APP_LIST, Payload::List(r, epoch));
        });
    }

    pub(crate) fn do_search(&mut self) {
        let kw = self.search_text().trim().to_string();
        if kw.is_empty() {
            self.select_nav(0);
            return;
        }
        let epoch = self.next_epoch();
        self.mode = Mode::Browse;
        self.scroll = 0;
        self.status = format!("正在搜索「{}」…", kw);
        self.loading = true;
        self.invalidate();
        let h = self.hwnd_usize();
        let token = self.token.clone();
        std::thread::spawn(move || {
            let r = net::search(&token, &kw).map(|v| (v, format!("搜索：{}", kw)));
            post(h, WM_APP_LIST, Payload::List(r, epoch));
        });
    }

    fn open_detail(&mut self, idx: usize) {
        if idx >= self.apps.len() {
            return;
        }
        self.detail_epoch += 1;
        let epoch = self.detail_epoch;
        self.mode = Mode::Detail;
        self.detail_index = idx;
        self.detail_extra = None;
        self.scroll = 0;
        self.status = "应用详情  ·  点击「下载安装」保存到 Downloads".into();
        self.invalidate();
        let h = self.hwnd_usize();
        let token = self.token.clone();
        let id = self.apps[idx].id.clone();
        std::thread::spawn(move || {
            post(
                h,
                WM_APP_DETAIL,
                Payload::Detail(net::details(&token, &id), epoch),
            );
        });
    }

    fn start_download(&mut self, idx: usize) {
        let Some(app) = self.apps.get(idx).cloned() else {
            return;
        };
        let url = self
            .detail_extra
            .as_ref()
            .and_then(|d| d.download_url.clone())
            .filter(|u| u.starts_with("http"))
            .unwrap_or_else(|| app.download_url());
        let h = self.hwnd_usize();
        self.status = format!("正在下载 {} …", app.name);
        self.invalidate();
        std::thread::spawn(move || {
            post(
                h,
                WM_APP_STATUS,
                Payload::Status(format!("正在下载 {} …", app.name)),
            );
            let result = updates_ui::download_package(&app.name, &url, h);
            post(h, WM_APP_DONE, Payload::Done(result, false));
        });
    }

    fn cols(&self) -> usize {
        let content_w = self.width as f32 - NAV_W - 2.0 * PAD;
        (((content_w + GAP) / (CARD_W + GAP)) as usize).max(1)
    }

    fn card_rect(&self, i: usize) -> Option<(f32, f32, f32, f32)> {
        let cols = self.cols();
        let row = i / cols;
        let col = i % cols;
        let x = NAV_W + PAD + col as f32 * (CARD_W + GAP);
        let y = TOP_H + 76.0 + row as f32 * (CARD_H + GAP) - self.scroll as f32;
        let h = self.height as f32;
        if y + CARD_H < TOP_H + 70.0 || y > h - STATUS_H {
            return None;
        }
        Some((x, y, x + CARD_W, y + CARD_H))
    }

    fn install_rect(card: (f32, f32, f32, f32)) -> D2D_RECT_F {
        gfx::rect(
            card.2 - 94.0,
            card.3 - 16.0 - BTN_H,
            card.2 - 16.0,
            card.3 - 16.0,
        )
    }

    fn back_rect() -> D2D_RECT_F {
        gfx::rect(NAV_W + PAD, TOP_H + 14.0, NAV_W + PAD + 84.0, TOP_H + 48.0)
    }

    fn download_rect() -> D2D_RECT_F {
        let tx = NAV_W + PAD + 128.0;
        gfx::rect(tx, TOP_H + 172.0, tx + 144.0, TOP_H + 208.0)
    }

    fn search_button_rect(w: f32) -> D2D_RECT_F {
        gfx::rect(w - 104.0, 16.0, w - PAD, 52.0)
    }

    fn nav_y(i: usize) -> f32 {
        match i {
            0 => TOP_H + 32.0,
            1 => TOP_H + 98.0,
            n => TOP_H + 164.0 + (n - 2) as f32 * NAV_ROW_H,
        }
    }

    fn nav_rect(i: usize) -> D2D_RECT_F {
        let y = Self::nav_y(i);
        gfx::rect(12.0, y, NAV_W - 12.0, y + 34.0)
    }

    fn detail_layout(&self, w: f32) -> DetailLayout {
        let a = self.apps.get(self.detail_index);
        let d = self.detail_extra.as_ref();
        let body = d
            .and_then(|x| x.detail_info.clone())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| a.map(|x| x.subtitle()))
            .unwrap_or_else(|| "暂无应用简介。".into());
        let body = body
            .replace("<br />", "\n")
            .replace("<br/>", "\n")
            .replace("<br>", "\n")
            .replace("&nbsp;", " ");
        let cw = (w - NAV_W - 2.0 * PAD - 14.0).max(180.0);
        let intro = wrap_text(&body, cw, 13);
        let intro_h = intro.lines().count().max(1) as f32 * 21.0 + 8.0;

        let wn = d
            .and_then(|x| x.what_new.clone())
            .unwrap_or_default()
            .trim()
            .to_string();
        let whats_new = if wn.is_empty() {
            String::new()
        } else {
            wrap_text(&wn, cw, 13)
        };
        let whats_new_h = if whats_new.is_empty() {
            0.0
        } else {
            whats_new.lines().count().max(1) as f32 * 21.0 + 8.0
        };

        let mut info = Vec::new();
        let version = d
            .and_then(|x| x.version.clone())
            .or_else(|| a.and_then(|x| x.version.clone()))
            .filter(|s| !s.trim().is_empty());
        if let Some(v) = version {
            info.push(("版本".into(), v));
        }
        let size = d
            .and_then(|x| x.size.clone())
            .or_else(|| a.and_then(|x| x.size.clone()))
            .filter(|s| !s.trim().is_empty());
        if let Some(v) = size {
            info.push(("安装包大小".into(), v));
        }
        let downloads = d
            .and_then(|x| x.downloads.clone())
            .or_else(|| a.and_then(|x| x.downloads.clone()))
            .filter(|s| !s.trim().is_empty());
        if let Some(v) = downloads {
            info.push(("安装量".into(), v));
        }
        let dev = d.map(|x| x.developer_name()).unwrap_or_default();
        if !dev.is_empty() {
            info.push(("开发者".into(), dev));
        }

        let total_h = 32.0
            + intro_h
            + 16.0
            + if whats_new_h > 0.0 {
                32.0 + whats_new_h + 16.0
            } else {
                0.0
            }
            + 30.0
            + info.len() as f32 * 26.0
            + 40.0;
        DetailLayout {
            intro,
            intro_h,
            whats_new,
            whats_new_h,
            info,
            total_h,
        }
    }

    fn max_scroll(&self) -> i32 {
        let h = self.height as f32;
        match self.mode {
            Mode::Browse => {
                let cols = self.cols();
                let rows = self.apps.len().div_ceil(cols);
                let content = rows as f32 * (CARD_H + GAP);
                let visible = (h - STATUS_H - TOP_H - 90.0).max(0.0);
                (content - visible).max(0.0) as i32
            }
            Mode::Updates => self.update_max_scroll(),
            Mode::Detail => {
                let layout = self.detail_layout(self.width as f32);
                let visible = (h - STATUS_H - TOP_H - 250.0).max(0.0);
                (layout.total_h - visible).max(0.0) as i32
            }
        }
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn render(&mut self) {
        unsafe {
            let mut rc = RECT::default();
            if GetClientRect(self.hwnd, &mut rc).is_ok() {
                let cw = ((rc.right - rc.left).max(1)) as u32;
                let ch = ((rc.bottom - rc.top).max(1)) as u32;
                if cw != self.gfx.phys_w || ch != self.gfx.phys_h {
                    self.gfx.resize(cw, ch);
                }
                let s = self.gfx.scale.max(0.5);
                self.width = ((cw as f32 / s) as u32).max(1);
                self.height = ((ch as f32 / s) as u32).max(1);
            }
        }
        let w = self.width as f32;
        let h = self.height as f32;
        let p = self.gfx.pal;
        self.gfx.begin();

        // Content background first; the header and nav paint over it.
        self.gfx
            .fill(gfx::rect(NAV_W, TOP_H, w, h - STATUS_H), p.bg);
        self.render_header(w);
        self.render_nav(h);

        // Everything below is clipped to the content pane, so scrolling text
        // cannot cover the header, navigation or status bar.
        self.gfx.push_clip(gfx::rect(NAV_W, TOP_H, w, h - STATUS_H));
        match self.mode {
            Mode::Detail => self.render_detail(w, h),
            Mode::Updates => self.render_updates(w, h),
            Mode::Browse => self.render_browse(w, h),
        }
        self.gfx.pop_clip();

        // Status bar
        self.gfx.fill(gfx::rect(0.0, h - STATUS_H, w, h), p.status);
        self.gfx.fill(
            gfx::rect(0.0, h - STATUS_H, w, h - STATUS_H + 1.0),
            p.border,
        );
        let status_w = (w - 190.0).max(120.0);
        self.gfx.text(
            &self.status,
            gfx::rect(16.0, h - STATUS_H + 2.0, status_w, h - 4.0),
            11,
            false,
            p.sub,
        );
        let version_label = format!("MindosShop v{}", env!("CARGO_PKG_VERSION"));
        self.gfx.text(
            &version_label,
            gfx::rect(w - 180.0, h - STATUS_H + 2.0, w - 16.0, h - 4.0),
            10,
            false,
            p.sub,
        );
        self.gfx.end();
    }

    fn render_header(&mut self, w: f32) {
        let p = self.gfx.pal;
        self.gfx.fill(gfx::rect(0.0, 0.0, w, TOP_H), p.panel);
        self.gfx
            .fill(gfx::rect(0.0, TOP_H - 1.0, w, TOP_H), p.border);

        let logo = gfx::rect(22.0, 18.0, 52.0, 48.0);
        self.gfx.round(logo, 9.0, p.accent);
        self.gfx.center_text("M", logo, 16, true, p.on_accent);
        self.gfx.text(
            "MindosShop",
            gfx::rect(64.0, 12.0, 240.0, 56.0),
            17,
            true,
            p.text,
        );

        let search_l = 252.0;
        let search_r = (w - 116.0).max(search_l + 140.0);
        let sr = gfx::rect(search_l, 16.0, search_r, 52.0);
        self.gfx.round(sr, 9.0, p.bg);
        self.gfx.outline(
            sr,
            if self.focus_search {
                p.accent
            } else {
                p.border
            },
            if self.focus_search { 1.5 } else { 1.0 },
        );
        let shown = if self.search.is_empty() {
            "搜索店内应用".to_string()
        } else {
            self.search_text()
        };
        self.gfx.text(
            &shown,
            gfx::rect(search_l + 14.0, 17.0, search_r - 12.0, 51.0),
            13,
            false,
            if self.search.is_empty() {
                p.sub
            } else {
                p.text
            },
        );
        if self.focus_search {
            let cx = search_l + 14.0 + estimate_width(&shown, 13);
            self.gfx.fill(gfx::rect(cx, 22.0, cx + 1.2, 46.0), p.accent);
        }
        let br = Self::search_button_rect(w);
        draw_button(
            &mut self.gfx,
            p,
            br,
            "搜索",
            true,
            self.hover == Some((HIT_SEARCH_BTN, 0)),
            false,
        );
    }

    fn render_nav(&mut self, h: f32) {
        let p = self.gfx.pal;
        self.gfx
            .fill(gfx::rect(0.0, TOP_H, NAV_W, h - STATUS_H), p.panel);
        self.gfx
            .fill(gfx::rect(NAV_W - 1.0, TOP_H, NAV_W, h - STATUS_H), p.border);
        self.gfx.text(
            "发现",
            gfx::rect(20.0, TOP_H + 8.0, NAV_W - 16.0, TOP_H + 28.0),
            10,
            true,
            p.sub,
        );
        self.gfx.text(
            "管理",
            gfx::rect(20.0, TOP_H + 74.0, NAV_W - 16.0, TOP_H + 94.0),
            10,
            true,
            p.sub,
        );
        if self.nav.len() > 2 {
            self.gfx.text(
                "分类",
                gfx::rect(20.0, TOP_H + 140.0, NAV_W - 16.0, TOP_H + 160.0),
                10,
                true,
                p.sub,
            );
        }
        for (i, n) in self.nav.iter().enumerate() {
            let r = Self::nav_rect(i);
            if r.top > h - STATUS_H - 8.0 {
                break;
            }
            let active = i == self.active_nav;
            let hovered = self.hover == Some((HIT_NAV, i));
            if active {
                self.gfx.round(r, 8.0, p.accent_soft);
            } else if hovered {
                self.gfx.round(r, 8.0, p.hover);
            }
            if active {
                self.gfx.round(
                    gfx::rect(12.0, r.top + 8.0, 15.0, r.bottom - 8.0),
                    1.5,
                    p.accent,
                );
            }
            self.gfx.text(
                &ellipsize(&n.label, r.right - 40.0, 13),
                gfx::rect(26.0, r.top, r.right - 34.0, r.bottom),
                13,
                active,
                if active { p.accent } else { p.text },
            );
            if n.kind == NavKind::Updates && !self.updates.is_empty() {
                let badge = gfx::rect(r.right - 30.0, r.top + 7.0, r.right - 8.0, r.top + 27.0);
                self.gfx.round(badge, 10.0, p.accent);
                self.gfx.center_text(
                    &self.updates.len().to_string(),
                    badge,
                    10,
                    true,
                    p.on_accent,
                );
            }
        }
    }

    fn render_browse(&mut self, w: f32, h: f32) {
        let p = self.gfx.pal;
        let title = if self.loading {
            "加载中…".to_string()
        } else {
            self.list_title.clone()
        };
        self.gfx.text(
            &title,
            gfx::rect(NAV_W + PAD, TOP_H + 12.0, w - PAD, TOP_H + 44.0),
            19,
            true,
            p.text,
        );
        self.gfx.text(
            &format!("共 {} 款应用  ·  纯净模式", self.apps.len()),
            gfx::rect(NAV_W + PAD, TOP_H + 44.0, w - PAD, TOP_H + 68.0),
            11,
            false,
            p.sub,
        );

        for (i, a) in self.apps.iter().enumerate() {
            let Some(card) = self.card_rect(i) else {
                continue;
            };
            let hovered = self.hover == Some((HIT_CARD, i)) || self.hover == Some((HIT_INSTALL, i));
            let (x0, y0, x1, y1) = card;
            self.gfx.round(
                gfx::rect(x0, y0, x1, y1),
                12.0,
                if hovered { p.hover } else { p.card },
            );
            self.gfx.outline(
                gfx::rect(x0, y0, x1, y1),
                p.border,
                if hovered { 1.2 } else { 0.8 },
            );

            let icon = gfx::rect(x0 + 16.0, y0 + 16.0, x0 + 72.0, y0 + 72.0);
            let key = a.logo_url().unwrap_or("");
            if !self.gfx.bitmap(key, icon) {
                self.gfx.round(icon, 12.0, p.accent_soft);
                let ch: String = a.name.chars().take(1).collect();
                self.gfx.center_text(&ch, icon, 22, true, p.accent);
            }

            let tx = x0 + 84.0;
            self.gfx.text(
                &ellipsize(&a.name, x1 - 14.0 - tx, 14),
                gfx::rect(tx, y0 + 14.0, x1 - 14.0, y0 + 38.0),
                14,
                true,
                p.text,
            );
            let desc = clip_lines(&a.subtitle(), x1 - 14.0 - tx - 4.0, 12, 2);
            self.gfx.text(
                &desc,
                gfx::rect(tx, y0 + 40.0, x1 - 14.0, y0 + 80.0),
                12,
                false,
                p.sub,
            );

            let ir = Self::install_rect(card);
            self.gfx.text(
                &ellipsize(&a.meta(), ir.left - 10.0 - (x0 + 16.0), 11),
                gfx::rect(x0 + 16.0, y0 + 88.0, ir.left - 10.0, y0 + 110.0),
                11,
                false,
                p.sub,
            );
            draw_button(
                &mut self.gfx,
                p,
                ir,
                "安装",
                true,
                self.hover == Some((HIT_INSTALL, i)),
                false,
            );
        }
        if self.apps.is_empty() && !self.loading {
            self.gfx.center_text(
                "没有找到应用",
                gfx::rect(NAV_W, TOP_H + 90.0, w, h - STATUS_H),
                16,
                false,
                p.sub,
            );
        }
    }

    fn render_detail(&mut self, w: f32, h: f32) {
        let p = self.gfx.pal;
        let Some(a) = self.apps.get(self.detail_index).cloned() else {
            return;
        };
        let d = self.detail_extra.as_ref();

        draw_button(
            &mut self.gfx,
            p,
            Self::back_rect(),
            "返回",
            false,
            self.hover == Some((HIT_BACK, 0)),
            false,
        );

        let icon = gfx::rect(
            NAV_W + PAD + 4.0,
            TOP_H + 66.0,
            NAV_W + PAD + 100.0,
            TOP_H + 162.0,
        );
        let key = a.logo_url().unwrap_or("");
        if !self.gfx.bitmap(key, icon) {
            self.gfx.round(icon, 18.0, p.accent_soft);
            let ch: String = a.name.chars().take(1).collect();
            self.gfx.center_text(&ch, icon, 42, true, p.accent);
        }

        let tx = NAV_W + PAD + 128.0;
        let right = (w - PAD).max(tx + 200.0);
        self.gfx.text(
            &ellipsize(&a.name, right - tx, 23),
            gfx::rect(tx, TOP_H + 68.0, right, TOP_H + 106.0),
            23,
            true,
            p.text,
        );
        let mut meta: Vec<String> = Vec::new();
        if let Some(v) = d
            .and_then(|x| x.version.clone())
            .or_else(|| a.version.clone())
            .filter(|s| !s.trim().is_empty())
        {
            meta.push(format!("版本 {}", v));
        }
        let m = a.meta();
        if !m.is_empty() {
            meta.push(m);
        }
        self.gfx.text(
            &ellipsize(&meta.join("  ·  "), right - tx, 12),
            gfx::rect(tx, TOP_H + 110.0, right, TOP_H + 134.0),
            12,
            false,
            p.sub,
        );
        let score = d.map(|x| x.score_text()).unwrap_or_else(|| a.score_text());
        let dev = d.map(|x| x.developer_name()).unwrap_or_default();
        let mut secondary = Vec::new();
        if !score.is_empty() {
            secondary.push(format!("评分 {}", score));
        }
        if !dev.is_empty() {
            secondary.push(dev);
        }
        if !secondary.is_empty() {
            self.gfx.text(
                &secondary.join("  ·  "),
                gfx::rect(tx, TOP_H + 138.0, right, TOP_H + 160.0),
                12,
                false,
                p.sub,
            );
        }
        draw_button(
            &mut self.gfx,
            p,
            Self::download_rect(),
            "下载安装",
            true,
            self.hover == Some((HIT_DOWNLOAD, 0)),
            false,
        );
        self.gfx.fill(
            gfx::rect(NAV_W + PAD, TOP_H + 226.0, w - PAD, TOP_H + 227.0),
            p.border,
        );

        // Scrolling body. A tall rect lets DrawText render every line while
        // the axis-aligned clip confines it to the content pane.
        let layout = self.detail_layout(w);
        let x0 = NAV_W + PAD;
        let x1 = (w - PAD - 10.0).max(x0 + 180.0);
        let mut y = TOP_H + 244.0 - self.scroll as f32;

        self.gfx
            .text("应用简介", gfx::rect(x0, y, x1, y + 26.0), 15, true, p.text);
        y += 32.0;
        self.gfx.text(
            &layout.intro,
            gfx::rect(x0, y, x1, y + layout.intro_h + 40.0),
            13,
            false,
            p.text,
        );
        y += layout.intro_h + 16.0;

        if layout.whats_new_h > 0.0 {
            self.gfx
                .text("本次更新", gfx::rect(x0, y, x1, y + 26.0), 15, true, p.text);
            y += 32.0;
            self.gfx.text(
                &layout.whats_new,
                gfx::rect(x0, y, x1, y + layout.whats_new_h + 40.0),
                13,
                false,
                p.sub,
            );
            y += layout.whats_new_h + 16.0;
        }

        if !layout.info.is_empty() {
            self.gfx
                .text("应用信息", gfx::rect(x0, y, x1, y + 26.0), 15, true, p.text);
            y += 30.0;
            for (k, v) in &layout.info {
                self.gfx
                    .text(k, gfx::rect(x0, y, x0 + 110.0, y + 24.0), 12, false, p.sub);
                self.gfx.text(
                    &ellipsize(v, x1 - x0 - 120.0, 12),
                    gfx::rect(x0 + 120.0, y, x1, y + 24.0),
                    12,
                    false,
                    p.text,
                );
                y += 26.0;
            }
        }

        let max = self.max_scroll() as f32;
        if max > 0.0 {
            let track_top = TOP_H + 238.0;
            let track_h = (h - STATUS_H - 16.0 - track_top).max(20.0);
            let thumb_h = (track_h * track_h / (track_h + max)).max(28.0);
            let t = (self.scroll as f32 / max).clamp(0.0, 1.0);
            let ty = track_top + (track_h - thumb_h) * t;
            self.gfx
                .round(gfx::rect(w - 9.0, ty, w - 5.0, ty + thumb_h), 2.0, p.subtle);
        }
    }

    // -----------------------------------------------------------------------
    // Hit testing: one source of truth for hover and click.
    // -----------------------------------------------------------------------

    fn hit_test(&self, x: f32, y: f32) -> Option<(u8, usize)> {
        let w = self.width as f32;
        let h = self.height as f32;
        if y < TOP_H {
            let search_r = (w - 116.0).max(392.0);
            if x >= 252.0 && x <= search_r {
                return Some((HIT_SEARCH_BOX, 0));
            }
            if in_rect(x, y, Self::search_button_rect(w)) {
                return Some((HIT_SEARCH_BTN, 0));
            }
            return None;
        }
        if y > h - STATUS_H {
            return None;
        }
        if x < NAV_W {
            for i in 0..self.nav.len() {
                if in_rect(x, y, Self::nav_rect(i)) {
                    return Some((HIT_NAV, i));
                }
            }
            return None;
        }
        match self.mode {
            Mode::Detail => {
                if in_rect(x, y, Self::back_rect()) {
                    return Some((HIT_BACK, 0));
                }
                if in_rect(x, y, Self::download_rect()) {
                    return Some((HIT_DOWNLOAD, 0));
                }
                None
            }
            Mode::Updates => updates_ui::hit_targets(self, x, y),
            Mode::Browse => {
                for i in 0..self.apps.len() {
                    if let Some(c) = self.card_rect(i)
                        && x >= c.0
                        && x <= c.2
                        && y >= c.1
                        && y <= c.3
                    {
                        if in_rect(x, y, Self::install_rect(c)) {
                            return Some((HIT_INSTALL, i));
                        }
                        return Some((HIT_CARD, i));
                    }
                }
                None
            }
        }
    }

    fn hit(&mut self, x: f32, y: f32) {
        let target = self.hit_test(x, y);
        self.focus_search = target == Some((HIT_SEARCH_BOX, 0));
        match target {
            Some((HIT_SEARCH_BTN, 0)) => self.do_search(),
            Some((HIT_NAV, i)) => {
                if self.token.is_empty() && self.nav[i].kind == NavKind::Browse {
                    self.status = "仍在初始化，请稍候…".into();
                    self.invalidate();
                } else {
                    self.select_nav(i);
                }
            }
            Some((HIT_BACK, 0)) => {
                self.mode = Mode::Browse;
                self.scroll = 0;
                self.invalidate();
            }
            Some((HIT_DOWNLOAD, 0)) => {
                let i = self.detail_index;
                self.start_download(i);
            }
            Some((HIT_INSTALL, i)) => self.start_download(i),
            Some((HIT_CARD, i)) => self.open_detail(i),
            Some((kind, i)) if (HIT_CHECK..=HIT_ROW).contains(&kind) => {
                updates_ui::apply(self, kind, i);
            }
            _ => self.invalidate(),
        }
    }

    // -----------------------------------------------------------------------

    pub fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                self.render();
                unsafe {
                    let _ = ValidateRect(Some(self.hwnd), None);
                }
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_SIZE => {
                self.width = (lparam.0 as u32) & 0xFFFF;
                self.height = ((lparam.0 as u32) >> 16) & 0xFFFF;
                if self.width > 0 && self.height > 0 {
                    self.gfx.resize(self.width, self.height);
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_GETMINMAXINFO => {
                unsafe {
                    let mm = lparam.0 as *mut MINMAXINFO;
                    if !mm.is_null() {
                        let s = self.gfx.scale.max(1.0);
                        (*mm).ptMinTrackSize.x = (MIN_W as f32 * s) as i32;
                        (*mm).ptMinTrackSize.y = (MIN_H as f32 * s) as i32;
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let s = self.gfx.scale.max(0.5);
                let x = ((lparam.0 as u32) & 0xFFFF) as i16 as f32 / s;
                let y = (((lparam.0 as u32) >> 16) & 0xFFFF) as i16 as f32 / s;
                self.hit(x, y);
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let s = self.gfx.scale.max(0.5);
                let x = ((lparam.0 as u32) & 0xFFFF) as i16 as f32 / s;
                let y = (((lparam.0 as u32) >> 16) & 0xFFFF) as i16 as f32 / s;
                let hover = self.hit_test(x, y);
                if hover != self.hover {
                    self.hover = hover;
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let max = self.max_scroll();
                self.scroll = (self.scroll - delta / 3).clamp(0, max);
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_CHAR => {
                if self.focus_search {
                    let c = (wparam.0 & 0xFFFF) as u16;
                    if c >= 0x20 && c != 0x7F {
                        self.search.push(c);
                        self.invalidate();
                    }
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => match wparam.0 {
                8 => {
                    if self.focus_search {
                        self.search.pop();
                        // Keep surrogate pairs intact when deleting emoji.
                        if let Some(&last) = self.search.last()
                            && (0xD800..=0xDBFF).contains(&last)
                        {
                            self.search.pop();
                        }
                        self.invalidate();
                    }
                    Some(LRESULT(0))
                }
                13 => {
                    if self.focus_search {
                        self.do_search();
                    }
                    Some(LRESULT(0))
                }
                27 => {
                    self.mode = Mode::Browse;
                    self.scroll = 0;
                    self.focus_search = false;
                    self.invalidate();
                    Some(LRESULT(0))
                }
                _ => None,
            },
            WM_APP_DATA
            | WM_APP_LIST
            | WM_APP_ICON
            | WM_APP_DETAIL
            | WM_APP_STATUS
            | WM_APP_DONE
            | WM_APP_UPDATE_PROGRESS
            | WM_APP_UPDATE_DONE => {
                self.on_payload(msg, lparam);
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn on_payload(&mut self, msg: u32, lparam: LPARAM) {
        if lparam.0 == 0 {
            return;
        }
        let p = unsafe { *Box::from_raw(lparam.0 as *mut Payload) };
        match (msg, p) {
            (WM_APP_DATA, Payload::Data(Ok((token, classes, apps)))) => {
                net::log(&format!(
                    "DATA ok classes={} apps={}",
                    classes.len(),
                    apps.len()
                ));
                self.token = token;
                self.load_nav(&classes);
                self.set_apps(apps, "推荐应用");
            }
            (WM_APP_DATA, Payload::Data(Err(e))) => {
                net::log(&format!("DATA err {}", e));
                self.loading = false;
                self.status = format!("连接失败：{}（检查网络后重启）", e);
                self.invalidate();
            }
            (WM_APP_LIST, Payload::List(Ok((apps, title)), epoch)) => {
                if epoch == self.list_epoch {
                    self.set_apps(apps, &title);
                }
            }
            (WM_APP_LIST, Payload::List(Err(e), epoch)) => {
                if epoch == self.list_epoch {
                    self.loading = false;
                    self.status = format!("加载失败：{}", e);
                    self.invalidate();
                }
            }
            (WM_APP_ICON, Payload::Icon { key, bytes }) => {
                self.pending_icons.remove(&key);
                if !bytes.is_empty() {
                    if self.gfx.icons.len() >= 200 {
                        self.gfx.icons.clear();
                    }
                    if let Some(bmp) = self.gfx.decode_icon(&bytes) {
                        self.gfx.icons.insert(key, bmp);
                        self.invalidate();
                    }
                }
            }
            (WM_APP_DETAIL, Payload::Detail(Ok(d), epoch)) => {
                if epoch == self.detail_epoch {
                    self.detail_extra = Some(d);
                    self.invalidate();
                }
            }
            (WM_APP_DETAIL, Payload::Detail(Err(e), epoch)) => {
                if epoch == self.detail_epoch {
                    net::log(&format!("DETAIL err {}", e));
                    self.status = format!("简介加载失败：{}", e);
                    self.invalidate();
                }
            }
            (WM_APP_STATUS, Payload::Status(s)) => {
                self.status = s;
                self.invalidate();
            }
            (WM_APP_DONE, Payload::Done(result, for_update)) => {
                self.status = match (result, for_update) {
                    (Ok(path), true) => format!(
                        "更新包已下载：{} — 请在 Downloads 中运行安装程序完成更新",
                        short_path(&path)
                    ),
                    (Ok(path), false) => format!(
                        "下载完成：{} — 请在 Downloads 中运行安装程序",
                        short_path(&path)
                    ),
                    (Err(e), _) => format!("下载失败：{}", e),
                };
                self.invalidate();
            }
            (WM_APP_UPDATE_PROGRESS, Payload::UpdateProgress(d, t)) => {
                self.on_update_progress(d, t)
            }
            (WM_APP_UPDATE_DONE, Payload::UpdateDone(r)) => self.on_update_done(r),
            _ => {}
        }
    }
}

pub(crate) fn in_rect(x: f32, y: f32, r: D2D_RECT_F) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

/// Kick off the initial network load on a worker thread.
pub fn start_load(hwnd_usize: usize) {
    std::thread::spawn(move || {
        let result = (|| -> Result<(String, Vec<ClassGroup>, Vec<App>), String> {
            let token = net::register()?;
            let classes = net::classes(&token)?;
            let apps = net::class_apps(&token, "soft", "-1", 36)?;
            Ok((token, classes, apps))
        })();
        post(hwnd_usize, WM_APP_DATA, Payload::Data(result));
    });
}

pub unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if ptr == 0 {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let st = &mut *(ptr as *mut AppState);
    if msg == WM_DESTROY {
        let _ = Box::from_raw(ptr as *mut AppState);
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        PostQuitMessage(0);
        return LRESULT(0);
    }
    st.handle(msg, wparam, lparam)
        .unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlaps(a: D2D_RECT_F, b: D2D_RECT_F) -> bool {
        a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
    }

    /// Regression for "description text covered by the install button".
    #[test]
    fn card_text_never_overlaps_the_install_button() {
        let card = (0.0, 0.0, CARD_W, CARD_H);
        let install = AppState::install_rect(card);
        let tx = 84.0;
        let name = gfx::rect(tx, 14.0, CARD_W - 14.0, 38.0);
        let desc = gfx::rect(tx, 40.0, CARD_W - 14.0, 80.0);
        let meta = gfx::rect(16.0, 88.0, install.left - 10.0, 110.0);
        assert!(!overlaps(name, install), "app name overlaps install button");
        assert!(
            !overlaps(desc, install),
            "description overlaps install button"
        );
        assert!(!overlaps(meta, install), "metadata overlaps install button");
        assert!(name.bottom <= desc.top, "name and description overlap");
        assert!(desc.bottom <= meta.top, "description and metadata overlap");
    }

    #[test]
    fn detail_header_sits_above_the_scrolling_body() {
        assert!(AppState::download_rect().bottom < TOP_H + 244.0);
        assert!(AppState::back_rect().bottom < TOP_H + 66.0);
    }

    #[test]
    fn header_search_and_button_do_not_overlap() {
        let w = 1100.0f32;
        let box_r = (w - 116.0).max(392.0);
        let search = gfx::rect(252.0, 16.0, box_r, 52.0);
        let button = AppState::search_button_rect(w);
        assert!(!overlaps(search, button));
        assert!(button.right <= w - 16.0);
    }

    #[test]
    fn long_descriptions_are_wrapped_and_clipped() {
        let long = "这是一款功能非常丰富的应用程序".repeat(12);
        let wrapped = wrap_text(&long, 180.0, 12);
        assert!(wrapped.lines().count() > 3);
        let clipped = clip_lines(&long, 180.0, 12, 2);
        assert_eq!(clipped.lines().count(), 2);
        assert!(clipped.ends_with('…'));
        assert!(ellipsize("超长的应用程序名称示例", 40.0, 14).ends_with('…'));
    }
}
