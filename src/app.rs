//! Window state, message loop handling, layout and interaction.
use std::collections::HashSet;
use std::ffi::c_void;
use std::fs::File;
use std::io::Write;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{InvalidateRect, ValidateRect};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, Gfx};
use crate::model::{App, ClassGroup, Detail};
use crate::net;

pub const WM_APP_DATA: u32 = 0x8000 + 1;
pub const WM_APP_LIST: u32 = 0x8000 + 2;
pub const WM_APP_ICON: u32 = 0x8000 + 3;
pub const WM_APP_DETAIL: u32 = 0x8000 + 4;
pub const WM_APP_STATUS: u32 = 0x8000 + 5;
pub const WM_APP_DONE: u32 = 0x8000 + 6;

const NAV_W: f32 = 180.0;
const TOP_H: f32 = 64.0;
const STATUS_H: f32 = 28.0;
const CARD_W: f32 = 252.0;
const CARD_H: f32 = 104.0;
const GAP: f32 = 12.0;

pub enum Payload {
    Data(Result<(String, Vec<ClassGroup>, Vec<App>), String>),
    List(Result<(Vec<App>, String), String>),
    Icon { key: String, bytes: Vec<u8> },
    Detail(Result<Detail, String>),
    Status(String),
    Done(Result<String, String>),
}

fn post(hwnd_usize: usize, msg: u32, p: Payload) {
    unsafe {
        let h = HWND(hwnd_usize as *mut c_void);
        let raw = Box::into_raw(Box::new(p)) as isize;
        let _ = PostMessageW(Some(h), msg, WPARAM(0), LPARAM(raw));
    }
}

#[derive(Clone)]
pub struct NavItem {
    pub label: String,
    pub code: String,
    pub id: String,
}

pub enum Mode {
    Browse,
    Detail,
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
    pub search: Vec<u16>,
    pub focus_search: bool,
    pub mode: Mode,
    pub detail_index: usize,
    pub detail_extra: Option<Detail>,
    pub scroll: i32,
    pub hover_card: Option<usize>,
    pub status: String,
    pub loading: bool,
    pending_icons: HashSet<String>,
}

impl AppState {
    pub fn new(hwnd: HWND, gfx: Gfx, width: u32, height: u32) -> Self {
        Self {
            hwnd,
            gfx,
            width,
            height,
            token: String::new(),
            nav: vec![NavItem {
                label: "推荐".into(),
                code: "soft".into(),
                id: "-1".into(),
            }],
            active_nav: 0,
            apps: Vec::new(),
            search: Vec::new(),
            focus_search: false,
            mode: Mode::Browse,
            detail_index: 0,
            detail_extra: None,
            scroll: 0,
            hover_card: None,
            status: "正在连接应用目录…".into(),
            loading: true,
            pending_icons: HashSet::new(),
        }
    }

    fn hwnd_usize(&self) -> usize {
        self.hwnd.0 as usize
    }

    fn search_text(&self) -> String {
        String::from_utf16_lossy(&self.search)
    }

    pub fn queue_icons(&mut self) {
        let mut queued = 0;
        for a in &self.apps {
            let url = match a.logo_url() {
                Some(u) => u.to_string(),
                None => continue,
            };
            if self.gfx.icons.contains_key(&url) || !self.pending_icons.insert(url.clone()) {
                continue;
            }
            let h = self.hwnd_usize();
            std::thread::spawn(move || {
                if let Ok(bytes) = net::fetch_binary(&url, None) {
                    if !bytes.is_empty() {
                        post(h, WM_APP_ICON, Payload::Icon { key: url, bytes });
                    }
                }
            });
            queued += 1;
            if queued >= 28 {
                break;
            }
        }
    }

    fn set_apps(&mut self, apps: Vec<App>, title: &str) {
        self.apps = apps;
        self.scroll = 0;
        self.mode = Mode::Browse;
        self.loading = false;
        self.status = format!("{}  ·  {} 款应用  ·  纯净模式（无运营位 / 无遥测）", title, self.apps.len());
        self.queue_icons();
        self.invalidate();
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn load_nav(&mut self, classes: &[ClassGroup]) {
        self.nav.clear();
        self.nav.push(NavItem { label: "推荐".into(), code: "soft".into(), id: "-1".into() });
        for g in classes {
            if g.code != "soft" {
                continue;
            }
            for c in g.list.iter().take(14) {
                if c.name == "全部" {
                    continue;
                }
                self.nav.push(NavItem {
                    label: c.name.clone(),
                    code: g.code.clone(),
                    id: c.id.clone(),
                });
            }
        }
        if let Some(g) = classes.iter().find(|x| x.code != "soft") {
            self.nav.push(NavItem {
                label: g.name.clone(),
                code: g.code.clone(),
                id: "-1".into(),
            });
        }
    }

    fn select_nav(&mut self, idx: usize) {
        if idx >= self.nav.len() {
            return;
        }
        let item = self.nav[idx].clone();
        self.active_nav = idx;
        self.focus_search = false;
        self.status = format!("正在加载「{}」…", item.label);
        self.loading = true;
        self.invalidate();
        let h = self.hwnd_usize();
        let token = self.token.clone();
        let label = item.label.clone();
        std::thread::spawn(move || {
            let r = net::class_apps(&token, &item.code, &item.id, 36).map(|v| (v, label));
            post(h, WM_APP_LIST, Payload::List(r));
        });
    }

    fn do_search(&mut self) {
        let kw = self.search_text().trim().to_string();
        if kw.is_empty() {
            self.select_nav(0);
            return;
        }
        self.status = format!("正在搜索「{}」…", kw);
        self.loading = true;
        self.invalidate();
        let h = self.hwnd_usize();
        let token = self.token.clone();
        std::thread::spawn(move || {
            let r = net::search(&token, &kw).map(|v| (v, format!("搜索：{}", kw)));
            post(h, WM_APP_LIST, Payload::List(r));
        });
    }

    fn open_detail(&mut self, idx: usize) {
        if idx >= self.apps.len() {
            return;
        }
        self.mode = Mode::Detail;
        self.detail_index = idx;
        self.detail_extra = None;
        self.status = "应用详情  ·  点击「下载安装」保存到 Downloads".into();
        self.invalidate();
        let h = self.hwnd_usize();
        let token = self.token.clone();
        let id = self.apps[idx].id.clone();
        std::thread::spawn(move || {
            post(h, WM_APP_DETAIL, Payload::Detail(net::details(&token, &id)));
        });
    }

    fn start_download(&mut self, idx: usize) {
        if idx >= self.apps.len() {
            return;
        }
        let app = self.apps[idx].clone();
        let url = app.download_url();
        let h = self.hwnd_usize();
        self.status = format!("正在下载 {} …", app.name);
        self.invalidate();
        std::thread::spawn(move || {
            post(h, WM_APP_STATUS, Payload::Status(format!("正在下载 {} …", app.name)));
            let safe: String = app
                .name
                .chars()
                .map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
                .collect();
            let dir = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
            let path = format!("{}\\Downloads\\{}.exe", dir, safe.trim());
            let result = (|| -> Result<String, String> {
                let mut file = File::create(&path).map_err(|e| e.to_string())?;
                let mut total = 0usize;
                let mut next = 1024 * 1024usize;
                let mut sink = |chunk: &[u8]| {
                    let _ = file.write_all(chunk);
                    total += chunk.len();
                    if total >= next {
                        next += 1024 * 1024;
                        post(h, WM_APP_STATUS, Payload::Status(format!("正在下载 {} … {:.1} MB", app.name, total as f64 / 1048576.0)));
                    }
                };
                net::fetch_binary(&url, Some(&mut sink))?;
                let _ = file.flush();
                Ok(path)
            })();
            post(h, WM_APP_DONE, Payload::Done(result));
        });
    }

    fn card_rect(&self, i: usize) -> Option<(f32, f32, f32, f32)> {
        let w = self.width as f32;
        let content_x = NAV_W + 16.0;
        let content_w = w - content_x - 16.0;
        let cols = (((content_w + GAP) / (CARD_W + GAP)) as usize).max(1);
        let row = i / cols;
        let col = i % cols;
        let x = content_x + col as f32 * (CARD_W + GAP);
        let y = TOP_H + 54.0 + row as f32 * (CARD_H + GAP) - self.scroll as f32;
        if y + CARD_H < TOP_H || y > self.height as f32 - STATUS_H {
            return None;
        }
        Some((x, y, x + CARD_W, y + CARD_H))
    }

    fn install_rect(card: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        (card.2 - 78.0, card.3 - 36.0, card.2 - 12.0, card.3 - 10.0)
    }

    pub fn render(&mut self) {
        // Direct2D scales DIPs by the system DPI, so layout coordinates are
        // logical while the render target is sized in physical pixels.
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

        // Top bar
        self.gfx.fill(gfx::rect(0.0, 0.0, w, TOP_H), p.panel);
        self.gfx.fill(gfx::rect(0.0, TOP_H - 1.0, w, TOP_H), p.border);
        self.gfx.text("MindosShop", gfx::rect(20.0, 8.0, 232.0, 56.0), 20, true, p.accent);

        let search_l = 244.0;
        let search_r = w - 124.0;
        let sr = gfx::rect(search_l, 13.0, search_r, 51.0);
        self.gfx.round(sr, 8.0, p.bg);
        self.gfx.outline(sr, if self.focus_search { p.accent } else { p.border }, 1.0);
        let shown = if self.search.is_empty() {
            "搜索店内应用".to_string()
        } else {
            self.search_text()
        };
        self.gfx.text(&shown, gfx::rect(search_l + 12.0, 14.0, search_r - 10.0, 50.0), 14, false, if self.search.is_empty() { p.sub } else { p.text });
        let br = gfx::rect(w - 112.0, 13.0, w - 16.0, 51.0);
        self.gfx.round(br, 8.0, p.accent);
        self.gfx.center_text("搜索", br, 14, true, p.on_accent);

        // Left navigation
        self.gfx.fill(gfx::rect(0.0, TOP_H, NAV_W, h - STATUS_H), p.panel);
        self.gfx.fill(gfx::rect(NAV_W - 1.0, TOP_H, NAV_W, h - STATUS_H), p.border);
        for (i, n) in self.nav.iter().enumerate() {
            let y = TOP_H + 12.0 + i as f32 * 38.0;
            if y + 34.0 > h - STATUS_H {
                break;
            }
            let r = gfx::rect(12.0, y, NAV_W - 12.0, y + 34.0);
            if i == self.active_nav {
                self.gfx.round(r, 6.0, p.hover);
                self.gfx.fill(gfx::rect(12.0, y + 6.0, 16.0, y + 28.0), p.accent);
            }
            self.gfx.text(&n.label, gfx::rect(28.0, y, NAV_W - 18.0, y + 34.0), 13, i == self.active_nav, if i == self.active_nav { p.accent } else { p.text });
        }

        // Content
        self.gfx.fill(gfx::rect(NAV_W, TOP_H, w, h - STATUS_H), p.bg);
        if matches!(self.mode, Mode::Detail) {
            self.render_detail(w, h);
        } else {
            let title = if self.loading { "加载中" } else { "应用" };
            self.gfx.text(title, gfx::rect(NAV_W + 16.0, TOP_H + 12.0, w - 20.0, TOP_H + 46.0), 18, true, p.text);
            for (i, a) in self.apps.iter().enumerate() {
                let card = match self.card_rect(i) {
                    Some(c) => c,
                    None => continue,
                };
                let is_hover = self.hover_card == Some(i);
                self.gfx.round(gfx::rect(card.0, card.1, card.2, card.3), 10.0, if is_hover { p.hover } else { p.card });
                self.gfx.outline(gfx::rect(card.0, card.1, card.2, card.3), p.border, 0.5);
                let icon = gfx::rect(card.0 + 14.0, card.1 + 14.0, card.0 + 70.0, card.1 + 70.0);
                let key = a.logo_url().unwrap_or("");
                if !self.gfx.bitmap(key, icon) {
                    self.gfx.round(icon, 12.0, p.hover);
                    let ch: String = a.name.chars().take(1).collect();
                    self.gfx.center_text(&ch, icon, 22, true, p.accent);
                }
                self.gfx.text(&a.name, gfx::rect(card.0 + 80.0, card.1 + 10.0, card.2 - 10.0, card.1 + 34.0), 15, true, p.text);
                self.gfx.text(&a.subtitle(), gfx::rect(card.0 + 80.0, card.1 + 34.0, card.2 - 10.0, card.1 + 56.0), 11, false, p.sub);
                self.gfx.text(&a.meta(), gfx::rect(card.0 + 14.0, card.1 + 76.0, card.2 - 90.0, card.1 + 96.0), 10, false, p.sub);
                let ir = Self::install_rect(card);
                self.gfx.round(gfx::rect(ir.0, ir.1, ir.2, ir.3), 6.0, p.accent);
                self.gfx.center_text("安装", gfx::rect(ir.0, ir.1, ir.2, ir.3), 12, true, p.on_accent);
            }
            if self.apps.is_empty() && !self.loading {
                self.gfx.center_text("没有找到应用", gfx::rect(NAV_W, TOP_H + 60.0, w, h - STATUS_H), 16, false, p.sub);
            }
        }

        // Status bar
        self.gfx.fill(gfx::rect(0.0, h - STATUS_H, w, h), p.status);
        self.gfx.text(&self.status, gfx::rect(12.0, h - STATUS_H, w - 12.0, h - 2.0), 11, false, p.sub);
        self.gfx.end();
    }

    fn render_detail(&mut self, w: f32, h: f32) {
        let p = self.gfx.pal;
        let a = match self.apps.get(self.detail_index) {
            Some(a) => a.clone(),
            None => return,
        };
        let back = gfx::rect(NAV_W + 16.0, TOP_H + 10.0, NAV_W + 96.0, TOP_H + 44.0);
        self.gfx.round(back, 6.0, p.card);
        self.gfx.center_text("返回", back, 13, false, p.text);
        let icon = gfx::rect(NAV_W + 32.0, TOP_H + 70.0, NAV_W + 152.0, TOP_H + 190.0);
        let key = a.logo_url().unwrap_or("");
        if !self.gfx.bitmap(key, icon) {
            self.gfx.round(icon, 20.0, p.hover);
            let ch: String = a.name.chars().take(1).collect();
            self.gfx.center_text(&ch, icon, 44, true, p.accent);
        }
        let tx = NAV_W + 180.0;
        self.gfx.text(&a.name, gfx::rect(tx, TOP_H + 70.0, w - 30.0, TOP_H + 112.0), 26, true, p.text);
        self.gfx.text(&a.meta(), gfx::rect(tx, TOP_H + 116.0, w - 30.0, TOP_H + 142.0), 12, false, p.sub);
        let score = a.score_text();
        if !score.is_empty() {
            self.gfx.text(&format!("评分 {}", score), gfx::rect(tx, TOP_H + 142.0, w - 30.0, TOP_H + 166.0), 12, false, p.sub);
        }
        let dl = gfx::rect(tx, TOP_H + 176.0, tx + 140.0, TOP_H + 216.0);
        self.gfx.round(dl, 8.0, p.accent);
        self.gfx.center_text("下载安装", dl, 14, true, p.on_accent);
        let intro = a.subtitle();
        self.gfx.text("应用简介", gfx::rect(NAV_W + 32.0, TOP_H + 220.0, w - 30.0, TOP_H + 250.0), 16, true, p.text);
        let body = self.detail_extra.as_ref().and_then(|d| d.detail_info.clone()).filter(|s| !s.trim().is_empty()).unwrap_or(intro);
        let body = body.replace("<br>", "\n").replace("<br/>", "\n").replace("&nbsp;", " ");
        self.gfx.text(&body, gfx::rect(NAV_W + 32.0, TOP_H + 254.0, w - 40.0, h - STATUS_H - 12.0), 13, false, p.text);
    }

    fn hit(&mut self, x: f32, y: f32) {
        let w = self.width as f32;
        if y < TOP_H {
            if x >= 244.0 && x <= w - 124.0 {
                self.focus_search = true;
                self.invalidate();
                return;
            }
            if x >= w - 112.0 && x <= w - 16.0 {
                self.do_search();
                return;
            }
            self.focus_search = false;
            return;
        }
        self.focus_search = false;
        if matches!(self.mode, Mode::Detail) {
            let back = gfx::rect(NAV_W + 16.0, TOP_H + 10.0, NAV_W + 96.0, TOP_H + 44.0);
            if in_rect(x, y, back) {
                self.mode = Mode::Browse;
                self.invalidate();
                return;
            }
            let dl = gfx::rect(NAV_W + 180.0, TOP_H + 176.0, NAV_W + 320.0, TOP_H + 216.0);
            if in_rect(x, y, dl) {
                let i = self.detail_index;
                self.start_download(i);
            }
            return;
        }
        if x < NAV_W {
            let idx = ((y - TOP_H - 12.0) / 38.0) as usize;
            if idx < self.nav.len() {
                if self.token.is_empty() {
                    self.status = "仍在初始化，请稍候…".into();
                    self.invalidate();
                } else {
                    self.select_nav(idx);
                }
            }
            return;
        }
        for (i, _) in self.apps.iter().enumerate() {
            if let Some(c) = self.card_rect(i) {
                if x >= c.0 && x <= c.2 && y >= c.1 && y <= c.3 {
                    let ir = Self::install_rect(c);
                    if x >= ir.0 && x <= ir.2 && y >= ir.1 && y <= ir.3 {
                        self.start_download(i);
                    } else {
                        self.open_detail(i);
                    }
                    return;
                }
            }
        }
    }

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
                let mut hover = None;
                if matches!(self.mode, Mode::Browse) && x >= NAV_W {
                    for (i, _) in self.apps.iter().enumerate() {
                        if let Some(c) = self.card_rect(i) {
                            if x >= c.0 && x <= c.2 && y >= c.1 && y <= c.3 {
                                hover = Some(i);
                                break;
                            }
                        }
                    }
                }
                if hover != self.hover_card {
                    self.hover_card = hover;
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
                self.scroll = (self.scroll - delta / 3).max(0);
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
                    self.focus_search = false;
                    self.invalidate();
                    Some(LRESULT(0))
                }
                _ => None,
            },
            WM_APP_DATA | WM_APP_LIST | WM_APP_ICON | WM_APP_DETAIL | WM_APP_STATUS | WM_APP_DONE => {
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
                net::log(&format!("DATA ok classes={} apps={}", classes.len(), apps.len()));
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
            (WM_APP_LIST, Payload::List(Ok((apps, title)))) => self.set_apps(apps, &title),
            (WM_APP_LIST, Payload::List(Err(e))) => {
                self.loading = false;
                self.status = format!("加载失败：{}", e);
                self.invalidate();
            }
            (WM_APP_ICON, Payload::Icon { key, bytes }) => {
                if let Some(bmp) = self.gfx.decode_icon(&bytes) {
                    self.gfx.icons.insert(key, bmp);
                    self.invalidate();
                }
            }
            (WM_APP_DETAIL, Payload::Detail(Ok(d))) => {
                self.detail_extra = Some(d);
                self.invalidate();
            }
            (WM_APP_STATUS, Payload::Status(s)) => {
                self.status = s;
                self.invalidate();
            }
            (WM_APP_DONE, Payload::Done(Ok(path))) => {
                self.status = format!("下载完成：{}", path);
                self.invalidate();
            }
            (WM_APP_DONE, Payload::Done(Err(e))) => {
                self.status = format!("下载失败：{}", e);
                self.invalidate();
            }
            _ => {}
        }
    }
}

fn in_rect(x: f32, y: f32, r: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F) -> bool {
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
