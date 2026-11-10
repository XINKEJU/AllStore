//! Window state, message loop handling, layout and interaction.
//!
//! Layout uses a single set of spacing tokens and one shared hit-test so the
//! rendering and the click targets can never drift apart.
use std::collections::{HashSet, VecDeque};
use std::ffi::c_void;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Gdi::{InvalidateRect, ScreenToClient, ValidateRect};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::downloads::{self, DlState, DlTask};
use crate::gfx::{self, Gfx, Palette};
use crate::installed::{InstalledApp, Inventory};
use crate::library;
use crate::model::{App, ClassGroup, Detail, html_to_text};
use crate::net;
use crate::update::{self, UpdateItem};
use crate::updates_ui;

pub const WM_APP_DATA: u32 = 0x8000 + 1;
pub const WM_APP_LIST: u32 = 0x8000 + 2;
pub const WM_APP_ICON: u32 = 0x8000 + 3;
pub const WM_APP_DETAIL: u32 = 0x8000 + 4;
pub const WM_APP_STATUS: u32 = 0x8000 + 5;
pub const WM_APP_UPDATE_PROGRESS: u32 = 0x8000 + 7;
pub const WM_APP_UPDATE_DONE: u32 = 0x8000 + 8;
pub const WM_APP_DL_PROGRESS: u32 = 0x8000 + 9;
pub const WM_APP_DL_DONE: u32 = 0x8000 + 10;
pub const WM_APP_FALLBACK: u32 = 0x8000 + 12;
/// Single 500 ms clock for caret blink and speed-graph sampling.
pub const TIMER_ID: usize = 1;

// Design tokens: one grid for every screen.
pub(crate) const NAV_W: f32 = 200.0;
pub(crate) const TOP_H: f32 = 72.0;
pub(crate) const STATUS_H: f32 = 40.0;
pub(crate) const PAD: f32 = 24.0;
pub(crate) const GAP: f32 = 14.0;
pub(crate) const CARD_W: f32 = 292.0;
pub(crate) const CARD_H: f32 = 176.0;
pub(crate) const BTN_H: f32 = 32.0;
pub(crate) const NAV_ROW_H: f32 = 40.0;
pub(crate) const ROW_H: f32 = 84.0;
pub(crate) const HERO_H: f32 = 88.0;
pub(crate) const MIN_W: i32 = 1000;
pub(crate) const MIN_H: i32 = 620;

// Detail page: the hero band and the scrolling body are separate regions.
// Keeping these as tokens makes it impossible for a progress bar to be drawn
// on top of a line of text (the 0.4.0 bug).
pub(crate) const DETAIL_HERO_TOP: f32 = TOP_H + 14.0;
pub(crate) const DETAIL_HERO_BOTTOM: f32 = TOP_H + 214.0;
pub(crate) const DETAIL_DIVIDER_Y: f32 = TOP_H + 230.0;
pub(crate) const DETAIL_BODY_TOP: f32 = TOP_H + 246.0;
// Compile-time guard: the hero card, its divider and the scrolling body must
// stay in strictly separate vertical bands.
const _: () = assert!(DETAIL_HERO_BOTTOM < DETAIL_DIVIDER_Y && DETAIL_DIVIDER_Y < DETAIL_BODY_TOP);

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
pub const HIT_DL_CANCEL: u8 = 12;
pub const HIT_DL_RETRY: u8 = 13;
pub const HIT_DL_OPEN: u8 = 14;
pub const HIT_DL_REMOVE: u8 = 15;
pub const HIT_DL_CLEAR: u8 = 16;
pub const HIT_DL_NAV: u8 = 17;
pub const HIT_IGNORED_LIST: u8 = 18;
pub const HIT_IGNORED_RESTORE: u8 = 19;
pub const HIT_IGNORED_ALL: u8 = 20;
pub const HIT_REINSTALL: u8 = 21;
pub const HIT_DL_INSTALL: u8 = 22;
/// Pause / resume a task from the download centre (Range-based continuation).
pub const HIT_DL_PAUSE: u8 = 23;
// "我的软件" local library page.
pub const HIT_LIB_OPEN: u8 = 24;
pub const HIT_LIB_LOCATE: u8 = 25;
pub const HIT_LIB_UPDATE: u8 = 26;
pub const HIT_LIB_UNINSTALL: u8 = 27;
// Source selector in the header.
pub const HIT_SOURCE: u8 = 28;
/// Rescan the registry inventory on the "我的软件" page.
pub const HIT_LIB_REFRESH: u8 = 32;

pub enum Payload {
    Data(Result<(String, Vec<ClassGroup>, Vec<App>), String>),
    /// (result, epoch) — stale responses from rapid navigation are discarded.
    List(Result<(Vec<App>, String), String>, u64),
    Icon {
        key: String,
        bytes: Vec<u8>,
    },
    Detail(Result<Detail, String>, u64),
    /// Download links that answered 404/410 and must leave the catalog.
    /// A failed download with a replacement link found on another source.
    Fallback {
        id: u64,
        url: Option<String>,
    },
    Status(String),
    DlProgress {
        id: u64,
        done: u64,
        total: u64,
        speed: f64,
        disk: f64,
    },
    DlDone {
        id: u64,
        result: Result<String, String>,
    },
    UpdateProgress(usize, usize),
    UpdateDone(Result<update::CheckResult, String>),
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
    Downloads,
    Library,
}

#[derive(Clone)]
pub struct NavItem {
    pub label: String,
    pub code: String,
    pub id: String,
    pub kind: NavKind,
    /// 0 = 发现, 1 = 管理, 2 = 分类. Group headers are derived from this.
    pub group: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browse,
    Detail,
    Updates,
    Downloads,
    Library,
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
    /// Unfiltered result list backing `apps`; switching the source chip only
    /// re-filters this in memory, so no request is issued.
    pub catalog_all: Vec<App>,
    /// The merged multi-source catalog, kept for the lifetime of the session.
    /// Going back to "推荐" restores it instead of re-querying Lenovo, which
    /// used to shrink the store to a single source after one category jump.
    pub home: Vec<App>,
    /// `None` = every source, otherwise a source display name.
    pub source_filter: Option<String>,
    /// Source names present in the current list, in first-seen order.
    pub sources: Vec<String>,
    /// Entries per source, parallel to `sources`. Shown on the chips so the
    /// selector reports what each source actually contributed.
    pub source_counts: Vec<usize>,
    pub list_title: String,
    pub search: Vec<u16>,
    pub focus_search: bool,
    /// UTF-16 index of the insertion point (not simply "end of string").
    pub search_caret: usize,
    /// Ctrl+A selection; the next keystroke replaces the whole query.
    pub search_select_all: bool,
    /// Blinking caret phase, driven by the 500 ms window timer.
    pub caret_on: bool,
    /// WM_CHAR delivers UTF-16 code units one at a time; hold a high surrogate
    /// until its pair arrives so emoji / rare CJK are not turned into U+FFFD.
    high_surrogate: Option<u16>,
    pub mode: Mode,
    pub detail_index: usize,
    pub detail_extra: Option<Detail>,
    pub scroll: i32,
    /// (hit kind, index) of the control under the pointer.
    pub hover: Option<(u8, usize)>,
    pub status: String,
    pub loading: bool,
    pub updates: Vec<UpdateItem>,
    /// Catalog snapshot from the last full check, used for instant local
    /// re-scans after the user installs an update.
    pub update_catalog: Vec<App>,
    pub update_checking: bool,
    pub update_progress: (usize, usize),
    pub update_checked_at: u64,
    pub ignored: Vec<String>,
    /// Display rows for ignored updates, so the restore page has names even
    /// when the catalog has not been fetched in this session.
    pub ignored_items: Vec<UpdateItem>,
    /// true = the updates page currently shows the ignored list.
    pub ignored_view: bool,
    /// Local registry snapshot (no I/O on paint; rebuilt on refresh).
    pub inventory: Inventory,
    pub(crate) last_local_scan: Instant,
    /// Download centre tasks; the single source of truth for every progress UI.
    pub downloads: Vec<DlTask>,
    /// Rows of the "我的软件" page, rebuilt from the registry snapshot.
    pub lib_rows: Vec<library::LibRow>,
    pub next_dl_id: u64,
    /// Rolling 36-second transfer history for the Steam-style speed graph.
    pub speed_history: VecDeque<downloads::SpeedSample>,
    list_epoch: u64,
    detail_epoch: u64,
    pending_icons: HashSet<String>,
}

// ---------------------------------------------------------------------------
// Layout helpers. Every rectangle used for drawing is produced here so the
// hit-test, the renderer and the regression tests all agree on one geometry.
// ---------------------------------------------------------------------------

/// A lone card with a huge empty gutter looks nothing like a store: keep at
/// least two columns and stretch every card to exactly fill the content pane.
fn layout_columns(width_dip: f32) -> (usize, f32) {
    let content_w = (width_dip - NAV_W - 2.0 * PAD).max(240.0);
    let cols = (((content_w + GAP) / (CARD_W + GAP)) as usize).clamp(2, 5);
    let cw = (content_w - (cols - 1) as f32 * GAP) / cols as f32;
    (cols, cw)
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
        let mut state = Self {
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
                    group: 0,
                },
                NavItem {
                    label: "软件更新".into(),
                    code: String::new(),
                    id: String::new(),
                    kind: NavKind::Updates,
                    group: 1,
                },
                NavItem {
                    label: "下载管理".into(),
                    code: String::new(),
                    id: String::new(),
                    kind: NavKind::Downloads,
                    group: 1,
                },
                NavItem {
                    label: "我的软件".into(),
                    code: String::new(),
                    id: String::new(),
                    kind: NavKind::Library,
                    group: 1,
                },
            ],
            active_nav: 0,
            apps: Vec::new(),
            catalog_all: Vec::new(),
            home: Vec::new(),
            source_filter: None,
            sources: Vec::new(),
            source_counts: Vec::new(),
            list_title: "推荐应用".into(),
            search: Vec::new(),
            focus_search: false,
            search_caret: 0,
            search_select_all: false,
            caret_on: true,
            high_surrogate: None,
            mode: Mode::Browse,
            detail_index: 0,
            detail_extra: None,
            scroll: 0,
            hover: None,
            status: "正在连接应用目录…".into(),
            loading: true,
            updates: cache.items.clone(),
            update_catalog: cache.catalog.clone(),
            update_checking: false,
            update_progress: (0, 0),
            update_checked_at: cache.checked_at,
            ignored: cache.ignored.clone(),
            ignored_items: cache.ignored_items.clone(),
            ignored_view: false,
            inventory: Inventory::default(),
            last_local_scan: Instant::now(),
            downloads: Vec::new(),
            lib_rows: Vec::new(),
            next_dl_id: 0,
            speed_history: VecDeque::with_capacity(downloads::SPEED_SAMPLES),
            list_epoch: 0,
            detail_epoch: 0,
            pending_icons: HashSet::new(),
        };
        // Restore the durable download queue BEFORE cleaning the temp folder,
        // so partials belonging to live tasks are kept and can be resumed.
        state.restore_downloads();
        let live_keys: Vec<String> = state
            .downloads
            .iter()
            .map(|t| updates_ui::part_key(&t.name, &t.url))
            .collect();
        updates_ui::cleanup_temp(&live_keys);
        // One registry walk at start-up feeds both the update badge and the
        // "installed" badges on every store card. No I/O happens while painting.
        state.refresh_updates_local();
        state
    }

    pub(crate) fn hwnd_usize(&self) -> usize {
        self.hwnd.0 as usize
    }

    fn search_text(&self) -> String {
        String::from_utf16_lossy(&self.search)
    }

    /// The input field is a real rectangle; the old hit-test treated the whole
    /// header strip as the field, so clicks near the title focused it.
    fn search_box_rect(&self) -> D2D_RECT_F {
        let w = self.width as f32;
        gfx::rect(252.0, 18.0, (w - 116.0).max(392.0), 54.0)
    }

    fn blur_search(&mut self) {
        self.focus_search = false;
        self.search_select_all = false;
        self.caret_on = false;
        self.high_surrogate = None;
    }

    /// Start of the UTF-16 character before `at` (never splits a surrogate).
    fn prev_boundary(units: &[u16], at: usize) -> usize {
        if at == 0 {
            return 0;
        }
        let mut i = at - 1;
        if (0xDC00..=0xDFFF).contains(&units[i]) && i > 0 {
            i -= 1;
        }
        i
    }

    fn next_boundary(units: &[u16], at: usize) -> usize {
        if at >= units.len() {
            return units.len();
        }
        let mut i = at + 1;
        if (0xD800..=0xDBFF).contains(&units[at])
            && i < units.len()
            && (0xDC00..=0xDFFF).contains(&units[i])
        {
            i += 1;
        }
        i
    }

    fn search_insert(&mut self, text: &str) {
        let units: Vec<u16> = text
            .encode_utf16()
            .filter(|c| *c >= 0x20 && *c != 0x7F)
            .collect();
        if units.is_empty() {
            return;
        }
        if self.search_select_all {
            self.search.clear();
            self.search_caret = 0;
            self.search_select_all = false;
        }
        let at = self.search_caret.min(self.search.len());
        self.search.splice(at..at, units.iter().copied());
        self.search_caret = at + units.len();
        self.caret_on = true;
        self.invalidate();
    }

    fn search_backspace(&mut self) {
        if self.search_select_all {
            self.search.clear();
            self.search_caret = 0;
            self.search_select_all = false;
        } else {
            let start = Self::prev_boundary(&self.search, self.search_caret);
            if start < self.search_caret {
                self.search.drain(start..self.search_caret);
                self.search_caret = start;
            }
        }
        self.caret_on = true;
        self.invalidate();
    }

    fn search_delete(&mut self) {
        if self.search_select_all {
            self.search.clear();
            self.search_caret = 0;
            self.search_select_all = false;
        } else {
            let end = Self::next_boundary(&self.search, self.search_caret);
            if end > self.search_caret {
                self.search.drain(self.search_caret..end);
            }
        }
        self.caret_on = true;
        self.invalidate();
    }

    fn search_move(&mut self, delta: isize) {
        self.search_select_all = false;
        let len = self.search.len();
        self.search_caret = if delta < 0 {
            Self::prev_boundary(&self.search, self.search_caret)
        } else {
            Self::next_boundary(&self.search, self.search_caret).min(len)
        };
        self.caret_on = true;
        self.invalidate();
    }

    fn search_move_edge(&mut self, end: bool) {
        self.search_select_all = false;
        self.search_caret = if end { self.search.len() } else { 0 };
        self.caret_on = true;
        self.invalidate();
    }

    /// Clicking inside the field positions the caret at the nearest measured
    /// glyph boundary instead of always appending to the end.
    fn search_set_caret_x(&mut self, x: f32) {
        const LEFT: f32 = 252.0 + 14.0;
        let target = (x - LEFT).max(0.0);
        let units = self.search.clone();
        let mut best = 0usize;
        let mut best_d = f32::MAX;
        for i in 0..=units.len() {
            if i > 0 && i < units.len() && (0xDC00..=0xDFFF).contains(&units[i]) {
                continue;
            }
            let prefix = String::from_utf16_lossy(&units[..i]);
            let w = self.gfx.measure_width(&prefix, 13, false);
            let d = (w - target).abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        self.search_caret = best;
        self.search_select_all = false;
        self.caret_on = true;
        self.high_surrogate = None;
    }

    fn paste_text(&mut self) {
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::DataExchange::*;
        use windows::Win32::System::Memory::*;
        const CF_UNICODETEXT: u32 = 13;
        unsafe {
            if OpenClipboard(Some(self.hwnd)).is_err() {
                return;
            }
            let text = (|| -> Option<String> {
                let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
                if handle.0.is_null() {
                    return None;
                }
                let mem = HGLOBAL(handle.0);
                let ptr = GlobalLock(mem) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                let mut len = 0usize;
                while *ptr.add(len) != 0 && len < 4096 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(mem);
                Some(s)
            })();
            let _ = CloseClipboard();
            if let Some(s) = text {
                let clean: String = s
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                self.search_insert(&clean);
            }
        }
    }

    /// Fetch logos through a small fixed worker pool. Spawning one thread per
    /// URL (28 at a time) saturated WinHTTP and made the first paint sluggish.
    pub fn queue_icons(&mut self) {
        let mut urls: Vec<String> = self
            .apps
            .iter()
            .filter_map(|a| a.logo_url().map(str::to_string))
            .collect();
        urls.extend(self.updates.iter().filter_map(|u| u.logo.clone()));
        let mut pending: Vec<String> = Vec::new();
        for url in urls {
            if self.gfx.icons.contains_key(&url) || !self.pending_icons.insert(url.clone()) {
                continue;
            }
            pending.push(url);
            if pending.len() >= 60 {
                break;
            }
        }
        if pending.is_empty() {
            return;
        }
        let workers = pending.len().min(6);
        let queue = Arc::new(Mutex::new(VecDeque::from(pending)));
        let h = self.hwnd_usize();
        for _ in 0..workers {
            let q = Arc::clone(&queue);
            std::thread::spawn(move || {
                loop {
                    let next = q.lock().ok().and_then(|mut g| g.pop_front());
                    let Some(url) = next else { break };
                    // An empty result still posts, so a failed icon can be retried.
                    let bytes = net::fetch_binary(&url).unwrap_or_default();
                    post(h, WM_APP_ICON, Payload::Icon { key: url, bytes });
                }
            });
        }
    }

    /// Rebuild the chip row (names + counts) from the catalog. Called whenever
    /// the catalog changes shape, including after dead links are removed.
    fn recompute_sources(&mut self) {
        let mut sources: Vec<String> = Vec::new();
        for a in &self.catalog_all {
            if !sources.iter().any(|s| s == a.origin()) {
                sources.push(a.origin().to_string());
            }
        }
        self.source_counts = sources
            .iter()
            .map(|s| self.catalog_all.iter().filter(|a| a.origin() == s).count())
            .collect();
        self.sources = sources;
        // A Lenovo-only category cannot honour a "腾讯" filter: fall back to
        // "all sources" instead of showing an empty page.
        if self
            .source_filter
            .as_ref()
            .is_some_and(|f| !self.sources.contains(f))
        {
            self.source_filter = None;
        }
    }

    fn set_apps(&mut self, apps: Vec<App>, title: &str) {
        self.catalog_all = apps;
        self.recompute_sources();
        self.apply_source_filter();
        self.list_title = title.to_string();
        self.scroll = 0;
        self.mode = Mode::Browse;
        self.loading = false;
        self.status = match &self.source_filter {
            Some(f) => format!(
                "{}  ·  仅看 {} 源  ·  共 {} 款应用",
                title,
                f,
                self.apps.len()
            ),
            None if self.sources.len() > 1 => format!(
                "{}  ·  {} 个源  ·  共 {} 款应用  ·  纯净模式（无运营位 / 无遥测）",
                title,
                self.sources.len(),
                self.apps.len()
            ),
            None => format!(
                "{}  ·  共 {} 款应用  ·  纯净模式（无运营位 / 无遥测）",
                title,
                self.apps.len()
            ),
        };
        self.queue_icons();
        self.invalidate();
    }

    /// Rebuild the visible list from the unfiltered one. Pure in-memory, so
    /// clicking a source chip is instant and never hits the network.
    pub(crate) fn apply_source_filter(&mut self) {
        self.apps = match &self.source_filter {
            Some(f) => self
                .catalog_all
                .iter()
                .filter(|a| a.origin() == f)
                .cloned()
                .collect(),
            None => self.catalog_all.clone(),
        };
        let max = self.max_scroll();
        self.scroll = self.scroll.min(max).max(0);
    }

    pub(crate) fn set_source_filter(&mut self, filter: Option<String>) {
        self.source_filter = filter;
        self.apply_source_filter();
        self.scroll = 0;
        self.status = match &self.source_filter {
            Some(f) => format!("仅显示 {} 源的应用  ·  {} 款", f, self.apps.len()),
            None => format!("显示全部软件源  ·  {} 款应用", self.apps.len()),
        };
        self.queue_icons();
        self.invalidate();
    }

    /// Source chips live in the hero banner, replacing the decorative tagline.
    /// Fixed-width chips keep the hit-test independent of glyph metrics.
    fn source_chip_rects(&self, w: f32) -> Vec<(D2D_RECT_F, Option<String>)> {
        source_chips(w, &self.sources, &self.source_counts)
    }

    pub(crate) fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn load_nav(&mut self, classes: &[ClassGroup]) {
        let previous = self
            .nav
            .get(self.active_nav)
            .map(|n| (n.kind.clone(), n.id.clone()));
        self.nav.clear();
        for (label, code, id, kind, group) in [
            ("推荐", "soft", "-1", NavKind::Browse, 0u8),
            ("软件更新", "", "", NavKind::Updates, 1u8),
            ("下载管理", "", "", NavKind::Downloads, 1u8),
            ("我的软件", "", "", NavKind::Library, 1u8),
        ] {
            self.nav.push(NavItem {
                label: label.into(),
                code: code.into(),
                id: id.into(),
                kind,
                group,
            });
        }
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
                    group: 2,
                });
            }
        }
        self.active_nav = previous
            .and_then(|(kind, id)| self.nav.iter().position(|n| n.kind == kind && n.id == id))
            .unwrap_or(0);
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
        match item.kind {
            NavKind::Updates => {
                self.mode = Mode::Updates;
                self.show_updates();
                return;
            }
            NavKind::Downloads => {
                self.mode = Mode::Downloads;
                self.status = format!(
                    "下载管理  ·  {} 个任务  ·  {} 个进行中",
                    self.downloads.len(),
                    self.active_count()
                );
                self.invalidate();
                return;
            }
            NavKind::Library => {
                self.show_library();
                return;
            }
            NavKind::Browse => {}
        }
        // The landing page already holds every source in memory; re-requesting
        // it would replace 736 rows with one Lenovo page.
        if idx == 0 && !self.home.is_empty() {
            self.set_apps(self.home.clone(), "推荐应用");
            return;
        }
        let epoch = self.next_epoch();
        self.status = format!("正在加载「{}」…", item.label);
        self.loading = true;
        self.invalidate();
        let h = self.hwnd_usize();
        let token = self.token.clone();
        let label = item.label.clone();
        // Rows of every other source that belong to this category, already in
        // memory: a category page is "Lenovo's page plus whoever else has a
        // matching category", with no extra request.
        let extra: Vec<App> = self
            .home
            .iter()
            .filter(|a| {
                a.category.as_deref() == Some(item.label.as_str()) && !a.id.starts_with('L')
            })
            .cloned()
            .collect();
        let cat = item.label.clone();
        std::thread::spawn(move || {
            let r = net::class_apps(&token, &item.code, &item.id, 36).map(|mut v| {
                for a in &mut v {
                    a.source = Some(crate::source::LENOVO.into());
                    a.category = Some(cat.clone());
                }
                v.extend(extra);
                (crate::source::dedup(v), label)
            });
            post(h, WM_APP_LIST, Payload::List(r, epoch));
        });
    }

    pub(crate) fn do_search(&mut self) {
        self.search_select_all = false;
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
        // Tencent and custom rows have no search endpoint, so their part of
        // the search is answered from the catalog already in memory.
        let local = self.catalog_all.clone();
        std::thread::spawn(move || {
            let r = net::search(&token, &kw).map(|mut remote| {
                for a in &mut remote {
                    if a.source.is_none() {
                        a.source = Some(crate::source::LENOVO.into());
                    }
                }
                for a in crate::source::search_local(&local, &kw) {
                    if !remote.iter().any(|x| x.id == a.id) {
                        remote.push(a);
                    }
                }
                (remote, format!("搜索：{}", kw))
            });
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
        self.status = "应用详情  ·  已装应用可直接打开，也可重新下载安装包".into();
        self.invalidate();
        let h = self.hwnd_usize();
        let token = self.token.clone();
        let id = self.apps[idx].id.clone();
        if !self.apps[idx].has_remote_detail() {
            // Only the Lenovo API has a details endpoint. For other sources
            // the list metadata is the truth; asking would just log an error.
            self.status = "应用详情  ·  数据来自聚合目录".into();
            self.invalidate();
            return;
        }
        std::thread::spawn(move || {
            post(
                h,
                WM_APP_DETAIL,
                Payload::Detail(net::details(&token, &id), epoch),
            );
        });
    }

    /// The primary action is state-aware: an active task cancels, a finished
    /// one opens the folder, an installed up-to-date app launches, an outdated
    /// app (or "re-download") fetches a package, everything else downloads.
    fn start_download(&mut self, idx: usize, force: bool) {
        let Some(app) = self.apps.get(idx).cloned() else {
            return;
        };
        let existing = self.task_for(&app.name).map(|t| (t.id, t.state));
        if let Some((id, state)) = existing {
            match state {
                // Pausing keeps the partial bytes on disk, so the same button
                // can continue later (and after a restart) instead of starting
                // the transfer over.
                DlState::Downloading | DlState::Queued => self.pause_task(id),
                DlState::Paused => self.resume_task(id),
                DlState::Failed | DlState::Canceled => self.retry_task(id),
                DlState::Done => {
                    // A finished primary action installs the package. The
                    // install path is verified by install_task; users who want
                    // just the file can use "打开位置" in the download centre.
                    self.install_task(id);
                }
            }
            return;
        }
        if !force {
            let version = if self.mode == Mode::Detail {
                self.detail_extra.as_ref().and_then(|d| d.version.clone())
            } else {
                None
            }
            .or_else(|| app.version.clone());
            if let Some(info) = self.inventory.status(&app.name, version.as_deref())
                && !info.outdated
            {
                if let Some(path) = info.launch {
                    self.launch_installed(&app.name, &path);
                } else {
                    let v = if info.version.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", info.version)
                    };
                    self.status = format!(
                        "{} 已在本机安装{}  ·  如需安装包，请在详情页点「重新下载」",
                        app.name, v
                    );
                }
                self.invalidate();
                return;
            }
        }
        let url = self
            .detail_extra
            .as_ref()
            .and_then(|d| d.download_url.clone())
            .filter(|u| u.starts_with("http"))
            .unwrap_or_else(|| app.download_url());
        let id = self.new_download(&app.name, &url, app.logo_url().map(str::to_string));
        self.start_task(id);
    }

    /// Only a verified `.exe` from the uninstall key reaches this function;
    /// the path is passed directly to CreateProcess, never through a shell.
    fn launch_installed(&mut self, name: &str, path: &Path) {
        let mut cmd = std::process::Command::new(path);
        if let Some(dir) = path.parent() {
            cmd.current_dir(dir);
        }
        self.status = match cmd.spawn() {
            Ok(_) => format!("已启动 {}", name),
            Err(e) => format!("无法启动 {}：{}  ·  可在详情页重新下载安装包", name, e),
        };
    }

    fn cols(&self) -> usize {
        layout_columns(self.width as f32).0
    }

    fn card_w(&self) -> f32 {
        layout_columns(self.width as f32).1
    }

    fn card_rect(&self, i: usize) -> Option<(f32, f32, f32, f32)> {
        let cols = self.cols();
        let cw = self.card_w();
        let row = i / cols;
        let col = i % cols;
        let x = NAV_W + PAD + col as f32 * (cw + GAP);
        let y = TOP_H + HERO_H + 28.0 + row as f32 * (CARD_H + GAP) - self.scroll as f32;
        let h = self.height as f32;
        if y + CARD_H < TOP_H + HERO_H + 16.0 || y > h - STATUS_H {
            return None;
        }
        Some((x, y, x + cw, y + CARD_H))
    }

    fn install_rect(card: (f32, f32, f32, f32)) -> D2D_RECT_F {
        gfx::rect(card.2 - 100.0, card.3 - 46.0, card.2 - 14.0, card.3 - 14.0)
    }

    fn card_name_rect(card: (f32, f32, f32, f32)) -> D2D_RECT_F {
        gfx::rect(card.0 + 92.0, card.1 + 12.0, card.2 - 16.0, card.1 + 38.0)
    }

    fn card_desc_rect(card: (f32, f32, f32, f32)) -> D2D_RECT_F {
        gfx::rect(card.0 + 92.0, card.1 + 40.0, card.2 - 16.0, card.1 + 80.0)
    }

    fn card_meta_rect(card: (f32, f32, f32, f32)) -> D2D_RECT_F {
        let ir = Self::install_rect(card);
        gfx::rect(card.0 + 16.0, card.1 + 98.0, ir.left - 14.0, card.1 + 120.0)
    }

    fn card_bar_rect(card: (f32, f32, f32, f32)) -> D2D_RECT_F {
        let ir = Self::install_rect(card);
        gfx::rect(
            card.0 + 16.0,
            card.1 + 132.0,
            ir.left - 14.0,
            card.1 + 138.0,
        )
    }

    fn card_status_rect(card: (f32, f32, f32, f32)) -> D2D_RECT_F {
        let ir = Self::install_rect(card);
        gfx::rect(
            card.0 + 16.0,
            card.1 + 142.0,
            ir.left - 14.0,
            card.1 + 162.0,
        )
    }

    fn back_rect() -> D2D_RECT_F {
        gfx::rect(
            NAV_W + PAD + 16.0,
            TOP_H + 26.0,
            NAV_W + PAD + 92.0,
            TOP_H + 58.0,
        )
    }

    fn download_rect(w: f32) -> D2D_RECT_F {
        gfx::rect(
            w - PAD - 190.0,
            TOP_H + 126.0,
            w - PAD - 24.0,
            TOP_H + 170.0,
        )
    }

    /// Own band below the progress caption — never crosses a line of text.
    fn download_progress_rect(w: f32) -> D2D_RECT_F {
        gfx::rect(
            NAV_W + PAD + 24.0,
            TOP_H + 194.0,
            w - PAD - 24.0,
            TOP_H + 200.0,
        )
    }

    fn detail_progress_text_rect(w: f32) -> D2D_RECT_F {
        // Full-width band *below* the primary action, so the installed-status
        // caption has room even on an 830 DIP window (0.7.0 regression: a
        // narrow right-hand column truncated "已安装 …" to an ellipsis).
        gfx::rect(
            NAV_W + PAD + 24.0,
            TOP_H + 174.0,
            w - PAD - 24.0,
            TOP_H + 192.0,
        )
    }

    fn reinstall_rect(w: f32) -> D2D_RECT_F {
        // Same column as the main action, one band above it.
        gfx::rect(w - PAD - 190.0, TOP_H + 88.0, w - PAD - 24.0, TOP_H + 118.0)
    }

    /// Shortcut in the featured banner.
    fn hero_dl_rect(w: f32) -> D2D_RECT_F {
        gfx::rect(w - PAD - 132.0, TOP_H + 42.0, w - PAD - 20.0, TOP_H + 76.0)
    }

    fn search_button_rect(w: f32) -> D2D_RECT_F {
        gfx::rect(w - 104.0, 18.0, w - PAD, 54.0)
    }

    fn detail_text_width(w: f32) -> f32 {
        (w - NAV_W - 2.0 * PAD - 10.0).max(180.0)
    }

    fn nav_y(&self, i: usize) -> f32 {
        let mut y = TOP_H + 8.0;
        let mut prev = u8::MAX;
        for (j, n) in self.nav.iter().enumerate() {
            if n.group != prev {
                y += 24.0;
                prev = n.group;
            }
            if j == i {
                break;
            }
            y += NAV_ROW_H;
        }
        y
    }

    fn nav_rect(&self, i: usize) -> D2D_RECT_F {
        let y = self.nav_y(i);
        gfx::rect(12.0, y, NAV_W - 12.0, y + 34.0)
    }

    fn group_title(group: u8) -> &'static str {
        match group {
            0 => "发现",
            1 => "管理",
            _ => "分类",
        }
    }

    /// Build the detail body using **measured** DirectWrite heights. The old
    /// code pre-wrapped with estimated character widths and reserved 21 px per
    /// line; when the real font wrapped differently, the extra lines painted
    /// straight over the next section heading.
    fn detail_layout(&mut self, w: f32) -> DetailLayout {
        let (body, wn, info) = {
            let a = self.apps.get(self.detail_index);
            let d = self.detail_extra.as_ref();
            let body = d
                .and_then(|x| x.detail_info.as_deref())
                .map(html_to_text)
                .filter(|s| !s.trim().is_empty())
                .or_else(|| a.map(|x| x.subtitle()))
                .unwrap_or_else(|| "暂无应用简介。".into());
            let wn = d
                .and_then(|x| x.what_new.as_deref())
                .map(html_to_text)
                .unwrap_or_default();
            let mut info: Vec<(String, String)> = Vec::new();
            let version = d
                .and_then(|x| x.version.clone())
                .or_else(|| a.and_then(|x| x.version.clone()))
                .filter(|s| !s.trim().is_empty());
            if let Some(v) = version.as_ref() {
                info.push(("版本".into(), v.clone()));
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
            if let Some(src) = a.map(|x| x.origin().to_string()) {
                info.push(("软件源".into(), src));
            }
            let local = a.and_then(|x| self.inventory.status(&x.name, version.as_deref()));
            match local {
                Some(st) => {
                    let mut text = if st.version.is_empty() {
                        "已安装".to_string()
                    } else {
                        format!("已安装  ·  {}", st.version)
                    };
                    if st.outdated {
                        text.push_str("  ·  有新版本");
                    }
                    info.push(("本机状态".into(), text));
                }
                None => info.push(("本机状态".into(), "未安装".into())),
            }
            (body, wn, info)
        };

        let cw = Self::detail_text_width(w);
        let intro_h = self.gfx.measure_height(&body, cw, 13, false, 0);
        let whats_new_h = if wn.trim().is_empty() {
            0.0
        } else {
            self.gfx.measure_height(&wn, cw, 13, false, 0)
        };
        let total_h = 34.0
            + intro_h
            + 22.0
            + if whats_new_h > 0.0 {
                34.0 + whats_new_h + 22.0
            } else {
                0.0
            }
            + 34.0
            + info.len() as f32 * 28.0
            + 56.0;
        DetailLayout {
            intro: body,
            intro_h,
            whats_new: wn,
            whats_new_h,
            info,
            total_h,
        }
    }

    fn max_scroll(&mut self) -> i32 {
        let h = self.height as f32;
        match self.mode {
            Mode::Browse => {
                let cols = self.cols();
                let rows = self.apps.len().div_ceil(cols);
                let content = rows as f32 * (CARD_H + GAP);
                let visible = (h - STATUS_H - TOP_H - HERO_H - 66.0).max(0.0);
                (content - visible).max(0.0) as i32
            }
            Mode::Updates => self.update_max_scroll(),
            Mode::Downloads => self.dl_max_scroll(),
            Mode::Library => self.lib_max_scroll(),
            Mode::Detail => {
                let layout = self.detail_layout(self.width as f32);
                let visible = (h - STATUS_H - DETAIL_BODY_TOP).max(0.0);
                (layout.total_h - visible).max(0.0) as i32
            }
        }
    }

    /// A thin, always-visible scroll indicator on every scrollable page.
    fn draw_scrollbar(&mut self, w: f32, h: f32) {
        let max = self.max_scroll() as f32;
        if max <= 0.0 {
            return;
        }
        let p = self.gfx.pal;
        let top = TOP_H + 14.0;
        let track_h = (h - STATUS_H - 16.0 - top).max(20.0);
        let thumb_h = (track_h * track_h / (track_h + max)).max(32.0);
        let t = (self.scroll as f32 / max).clamp(0.0, 1.0);
        let ty = top + (track_h - thumb_h) * t;
        self.gfx.round(
            gfx::rect(w - 10.0, ty, w - 6.0, ty + thumb_h),
            2.0,
            p.subtle,
        );
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
            Mode::Downloads => self.render_downloads(w, h),
            Mode::Library => self.render_library(w, h),
            Mode::Browse => self.render_browse(w, h),
        }
        self.draw_scrollbar(w, h);
        self.gfx.pop_clip();

        // Status bar: message on the left, live download progress in the
        // middle and a permanent shortcut into the download centre.
        self.gfx.fill(gfx::rect(0.0, h - STATUS_H, w, h), p.status);
        self.gfx.fill(
            gfx::rect(0.0, h - STATUS_H, w, h - STATUS_H + 1.0),
            p.border,
        );
        let status_w = (w - 430.0).max(160.0);
        let status = self.status.clone();
        self.gfx.text(
            &status,
            gfx::rect(16.0, h - STATUS_H, status_w, h),
            11,
            false,
            p.sub,
        );
        self.render_status_progress(w, h);
        let sr = Self::status_dl_rect(w, h);
        let active = self.active_count();
        draw_button(
            &mut self.gfx,
            p,
            sr,
            &if active > 0 {
                format!("下载 {}", active)
            } else {
                "下载管理".into()
            },
            self.mode == Mode::Downloads,
            self.hover == Some((HIT_DL_NAV, 0)),
            false,
        );
        // Present also recovers from a lost D2D device.
        self.gfx.present();
    }

    fn status_dl_rect(w: f32, h: f32) -> D2D_RECT_F {
        gfx::rect(w - 182.0, h - STATUS_H + 6.0, w - 16.0, h - 6.0)
    }

    fn render_header(&mut self, w: f32) {
        let p = self.gfx.pal;
        self.gfx.fill(gfx::rect(0.0, 0.0, w, TOP_H), p.panel);
        self.gfx
            .fill(gfx::rect(0.0, TOP_H - 1.0, w, TOP_H), p.border);

        let logo = gfx::rect(20.0, 18.0, 56.0, 54.0);
        self.gfx.round(logo, 10.0, p.accent);
        self.gfx.center_text("A", logo, 18, true, p.on_accent);
        self.gfx.text(
            "AllStore",
            gfx::rect(66.0, 12.0, 240.0, 38.0),
            17,
            true,
            p.text,
        );
        self.gfx.text(
            "发现 · 获取 · 更新",
            gfx::rect(66.0, 36.0, 240.0, 58.0),
            10,
            false,
            p.sub,
        );

        let sr = self.search_box_rect();
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
        let text_r = gfx::rect(sr.left + 14.0, 19.0, sr.right - 12.0, 53.0);
        let shown = self.search_text();
        if self.search.is_empty() {
            // Placeholder only while unfocused; a focused empty field shows a
            // real caret at the left edge instead of "placeholder + caret".
            if !self.focus_search {
                self.gfx.text("搜索店内应用", text_r, 13, false, p.sub);
            }
        } else {
            if self.search_select_all {
                let tw = self.gfx.measure_width(&shown, 13, false);
                self.gfx.fill(
                    gfx::rect(
                        sr.left + 14.0,
                        23.0,
                        (sr.left + 14.0 + tw).min(sr.right - 10.0),
                        48.0,
                    ),
                    p.accent_soft,
                );
            }
            self.gfx.text(&shown, text_r, 13, false, p.text);
        }
        if self.focus_search && self.caret_on {
            let at = self.search_caret.min(self.search.len());
            let prefix = String::from_utf16_lossy(&self.search[..at]);
            let cx = sr.left + 14.0 + self.gfx.measure_width(&prefix, 13, false);
            let cx = cx.min(sr.right - 8.0).max(sr.left + 14.0);
            self.gfx.fill(gfx::rect(cx, 22.0, cx + 1.4, 46.0), p.accent);
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
        let rects: Vec<D2D_RECT_F> = (0..self.nav.len()).map(|i| self.nav_rect(i)).collect();
        let active_downloads = self.active_count();
        let labels: Vec<String> = self.nav.iter().map(|n| n.label.clone()).collect();
        let groups: Vec<u8> = self.nav.iter().map(|n| n.group).collect();
        let badges: Vec<usize> = self
            .nav
            .iter()
            .map(|n| match n.kind {
                NavKind::Updates => self.updates.len(),
                NavKind::Downloads => active_downloads,
                NavKind::Browse | NavKind::Library => 0,
            })
            .collect();
        // Reserve the footer band: without this the last category row was
        // drawn straight through the version label on short windows.
        let footer_top = h - STATUS_H - 40.0;
        let mut prev_group = u8::MAX;
        for i in 0..self.nav.len() {
            let r = rects[i];
            if r.bottom > footer_top {
                break;
            }
            if groups[i] != prev_group {
                prev_group = groups[i];
                self.gfx.text(
                    Self::group_title(groups[i]),
                    gfx::rect(20.0, r.top - 20.0, NAV_W - 16.0, r.top - 2.0),
                    10,
                    true,
                    p.sub,
                );
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
            let badge_count = badges[i];
            let label_right = if badge_count > 0 {
                r.right - 40.0
            } else {
                r.right - 12.0
            };
            self.gfx.text(
                &labels[i],
                gfx::rect(26.0, r.top, label_right, r.bottom),
                13,
                active,
                if active { p.accent } else { p.text },
            );
            if badge_count > 0 {
                let badge = gfx::rect(r.right - 34.0, r.top + 7.0, r.right - 8.0, r.top + 27.0);
                self.gfx.round(badge, 10.0, p.accent);
                self.gfx
                    .center_text(&badge_count.to_string(), badge, 10, true, p.on_accent);
            }
        }
        self.gfx.fill(
            gfx::rect(12.0, footer_top, NAV_W - 12.0, footer_top + 1.0),
            p.border,
        );
        let version = format!("AllStore v{}", env!("CARGO_PKG_VERSION"));
        self.gfx.text(
            &version,
            gfx::rect(20.0, footer_top + 4.0, NAV_W - 16.0, h - STATUS_H - 6.0),
            10,
            false,
            p.sub,
        );
    }

    fn render_browse(&mut self, w: f32, _h: f32) {
        let p = self.gfx.pal;
        let title = if self.loading {
            "正在连接应用目录…".to_string()
        } else {
            self.list_title.clone()
        };

        // Featured banner — the store-like landing area, with a shortcut to
        // the download centre so progress is never more than one click away.
        let hero = gfx::rect(NAV_W + PAD, TOP_H + 14.0, w - PAD, TOP_H + 14.0 + HERO_H);
        if hero.right > hero.left + 200.0 {
            self.gfx.round(hero, 16.0, p.accent_soft);
            self.gfx.outline(hero, p.border, 0.8);
            // Three non-overlapping columns: title/subtitle | stats | action.
            let btn = Self::hero_dl_rect(w);
            let stat_right = btn.left - 20.0;
            // Wide enough for "736 款应用 · 12 个分类 · 7 个可更新" on one line.
            let stat_left = stat_right - 236.0;
            let text_right = stat_left - 16.0;
            self.gfx.text(
                &title,
                gfx::rect(
                    hero.left + 24.0,
                    hero.top + 12.0,
                    text_right,
                    hero.top + 48.0,
                ),
                19,
                true,
                p.text,
            );
            // Source chips replace the old static tagline: they show where the
            // catalog comes from and double as a one-click filter.
            let chips = self.source_chip_rects(w);
            let filter = self.source_filter.clone();
            let hover = self.hover;
            for (i, (r, src)) in chips.iter().enumerate() {
                let active = *src == filter;
                let label = chip_label(&self.sources, &self.source_counts, i);
                self.gfx
                    .round(*r, 9.0, if active { p.accent } else { p.bg });
                if !active {
                    self.gfx.outline(*r, p.border, 0.8);
                }
                if hover == Some((HIT_SOURCE, i)) {
                    self.gfx.outline(*r, p.accent, 1.4);
                }
                self.gfx.center_text(
                    &label,
                    *r,
                    11,
                    true,
                    if active { p.on_accent } else { p.text },
                );
            }
            if chips.is_empty() {
                self.gfx.text(
                    "多源聚合",
                    gfx::rect(
                        hero.left + 24.0,
                        hero.top + 48.0,
                        text_right,
                        hero.top + 74.0,
                    ),
                    11,
                    false,
                    p.sub,
                );
            }
            // One stats line in the title band: the bottom band belongs to the
            // source chips, which need the width far more than the numbers do.
            self.gfx.text(
                &format!(
                    "{} 款应用  ·  {} 个分类  ·  {} 个可更新",
                    self.apps.len(),
                    self.nav.len().saturating_sub(3),
                    self.updates.len()
                ),
                gfx::rect(stat_left, hero.top + 14.0, stat_right, hero.top + 40.0),
                12,
                false,
                p.sub,
            );
            draw_button(
                &mut self.gfx,
                p,
                btn,
                "下载管理",
                false,
                self.hover == Some((HIT_DL_NAV, 0)),
                false,
            );
        }

        // Resolve task states up front: rendering needs &mut gfx while the
        // task list is borrowed immutably.
        let states: Vec<Option<(DlState, f32, String)>> = self
            .apps
            .iter()
            .map(|a| {
                self.task_for(&a.name)
                    .map(|t| (t.state, t.frac(), t.percent()))
            })
            .collect();
        let local_states: Vec<Option<InstalledApp>> = self
            .apps
            .iter()
            .map(|a| self.inventory.status(&a.name, a.version.as_deref()))
            .collect();

        for (i, a) in self.apps.iter().enumerate() {
            let Some(card) = self.card_rect(i) else {
                continue;
            };
            let hovered = self.hover == Some((HIT_CARD, i)) || self.hover == Some((HIT_INSTALL, i));
            let (x0, y0, x1, y1) = card;
            self.gfx.round(
                gfx::rect(x0, y0, x1, y1),
                14.0,
                if hovered { p.hover } else { p.card },
            );
            self.gfx.outline(
                gfx::rect(x0, y0, x1, y1),
                if hovered { p.accent } else { p.border },
                if hovered { 1.4 } else { 0.9 },
            );

            let icon = gfx::rect(x0 + 16.0, y0 + 16.0, x0 + 80.0, y0 + 80.0);
            let key = a.logo_url().unwrap_or("");
            if !self.gfx.bitmap(key, icon) {
                self.gfx.round(icon, 14.0, p.accent_soft);
                let ch: String = a.name.chars().take(1).collect();
                self.gfx.center_text(&ch, icon, 24, true, p.accent);
            }

            let name_r = Self::card_name_rect(card);
            self.gfx.text(&a.name, name_r, 15, true, p.text);
            let desc_r = Self::card_desc_rect(card);
            let desc = a.subtitle();
            self.gfx.paragraph(&desc, desc_r, 12, false, p.sub, 2);
            self.gfx.fill(
                gfx::rect(x0 + 16.0, y0 + 88.0, x1 - 16.0, y0 + 89.0),
                p.border,
            );

            // Rating chip + price / size / install count, sized with real
            // DirectWrite metrics rather than per-character guesses.
            let meta_r = Self::card_meta_rect(card);
            let score = a.score_text();
            let mut cx = meta_r.left;
            if !score.is_empty() {
                let label = format!("★ {}", score);
                let cw = (self.gfx.measure_width(&label, 11, true) + 18.0)
                    .min(meta_r.right - meta_r.left - 8.0);
                self.gfx.round(
                    gfx::rect(cx, meta_r.top + 1.0, cx + cw, meta_r.bottom - 1.0),
                    9.0,
                    p.accent_soft,
                );
                self.gfx.text(
                    &label,
                    gfx::rect(cx + 7.0, meta_r.top, cx + cw - 5.0, meta_r.bottom),
                    11,
                    true,
                    p.accent,
                );
                cx += cw + 8.0;
            }
            let meta = a.meta();
            let price_line = if meta.is_empty() {
                "免费".to_string()
            } else {
                format!("免费  ·  {}", meta)
            };
            self.gfx.text(
                &price_line,
                gfx::rect(cx, meta_r.top, meta_r.right, meta_r.bottom),
                11,
                false,
                p.sub,
            );

            let ir = Self::install_rect(card);
            let task = states.get(i).and_then(|x| x.as_ref());
            if let Some((state, frac, pct)) = task {
                downloads::progress_bar(
                    &mut self.gfx,
                    p,
                    Self::card_bar_rect(card),
                    *frac,
                    if *state == DlState::Failed {
                        p.danger
                    } else {
                        p.accent
                    },
                );
                let text = match state {
                    DlState::Downloading => format!("{}  ·  下载中", pct),
                    DlState::Queued => "等待下载…".into(),
                    DlState::Paused => format!("{}  ·  已暂停，点击继续", pct),
                    DlState::Done => "已下载  ·  点击安装".into(),
                    DlState::Failed => "下载失败，可重试".into(),
                    DlState::Canceled => "已取消，可重试".into(),
                };
                self.gfx.text(
                    &text,
                    Self::card_status_rect(card),
                    11,
                    false,
                    if *state == DlState::Failed {
                        p.danger
                    } else if *state == DlState::Done {
                        p.accent
                    } else {
                        p.sub
                    },
                );
            } else if let Some(info) = local_states.get(i).and_then(|x| x.as_ref()) {
                let version = if info.version.is_empty() {
                    String::new()
                } else {
                    format!(" {}", info.version)
                };
                let text = if info.outdated {
                    format!("已安装{}  ·  有新版本", version)
                } else {
                    format!("已安装{}  ·  已是最新", version)
                };
                self.gfx.text(
                    &text,
                    Self::card_status_rect(card),
                    11,
                    false,
                    if info.outdated { p.accent } else { p.sub },
                );
            } else {
                self.gfx.text(
                    "官方目录  ·  安全下载",
                    Self::card_status_rect(card),
                    11,
                    false,
                    p.sub,
                );
            }
            let local = task
                .is_none()
                .then(|| local_states.get(i).and_then(|x| x.as_ref()))
                .flatten();
            let label = match task.map(|t| t.0) {
                Some(DlState::Downloading | DlState::Queued) => "暂停",
                Some(DlState::Paused) => "继续",
                Some(DlState::Done) => "安装",
                Some(DlState::Failed | DlState::Canceled) => "重试",
                None => match local {
                    Some(info) if info.outdated => "更新",
                    Some(info) if info.launch.is_some() => "打开",
                    Some(_) => "已安装",
                    None => "获取",
                },
            };
            draw_button(
                &mut self.gfx,
                p,
                ir,
                label,
                local.is_none_or(|x| x.outdated),
                self.hover == Some((HIT_INSTALL, i)),
                false,
            );
        }
        if self.apps.is_empty() && !self.loading {
            let card = gfx::rect(
                NAV_W + PAD,
                TOP_H + HERO_H + 40.0,
                w - PAD,
                TOP_H + HERO_H + 168.0,
            );
            self.gfx.round(card, 16.0, p.accent_soft);
            self.gfx.outline(card, p.border, 0.8);
            self.gfx.text(
                "没有找到应用",
                gfx::rect(
                    card.left + 28.0,
                    card.top + 24.0,
                    card.right - 28.0,
                    card.top + 62.0,
                ),
                17,
                true,
                p.text,
            );
            self.gfx.text(
                "换个关键词试试，或从左侧选择一个分类",
                gfx::rect(
                    card.left + 28.0,
                    card.top + 64.0,
                    card.right - 28.0,
                    card.top + 96.0,
                ),
                12,
                false,
                p.sub,
            );
        }
    }

    fn render_detail(&mut self, w: f32, _h: f32) {
        let p = self.gfx.pal;
        let Some(a) = self.apps.get(self.detail_index).cloned() else {
            return;
        };
        let d = self.detail_extra.clone();
        let task = self
            .task_for(&a.name)
            .map(|t| (t.state, t.frac(), t.percent()));
        let shop_version = d
            .as_ref()
            .and_then(|x| x.version.clone())
            .or_else(|| a.version.clone());
        let local = self.inventory.status(&a.name, shop_version.as_deref());

        // Hero card: big icon, identity, metadata and one prominent action.
        let hero = gfx::rect(NAV_W + PAD, DETAIL_HERO_TOP, w - PAD, DETAIL_HERO_BOTTOM);
        self.gfx.round(hero, 18.0, p.accent_soft);
        self.gfx.outline(hero, p.border, 0.8);
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
            hero.left + 24.0,
            hero.top + 54.0,
            hero.left + 120.0,
            hero.top + 150.0,
        );
        let key = a.logo_url().unwrap_or("");
        if !self.gfx.bitmap(key, icon) {
            self.gfx.round(icon, 20.0, p.accent_soft);
            let ch: String = a.name.chars().take(1).collect();
            self.gfx.center_text(&ch, icon, 44, true, p.accent);
        }

        let tx = hero.left + 140.0;
        let right = w - PAD - 214.0;
        self.gfx.text(
            &a.name,
            gfx::rect(tx, hero.top + 48.0, right, hero.top + 88.0),
            24,
            true,
            p.text,
        );
        let mut meta: Vec<String> = Vec::new();
        if let Some(v) = d
            .as_ref()
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
            &meta.join("  ·  "),
            gfx::rect(tx, hero.top + 92.0, right, hero.top + 114.0),
            12,
            false,
            p.sub,
        );
        let score = d
            .as_ref()
            .map(|x| x.score_text())
            .unwrap_or_else(|| a.score_text());
        let dev = d.as_ref().map(|x| x.developer_name()).unwrap_or_default();
        let mut secondary = Vec::new();
        if !score.is_empty() {
            secondary.push(format!("★ {} 分", score));
        }
        if !dev.is_empty() {
            secondary.push(dev);
        }
        secondary.push("免费".into());
        self.gfx.text(
            &secondary.join("  ·  "),
            gfx::rect(tx, hero.top + 118.0, right, hero.top + 140.0),
            12,
            false,
            p.sub,
        );

        let hint = Self::detail_progress_text_rect(w);
        if let Some((state, _frac, pct)) = &task {
            let caption = if *state == DlState::Done {
                "已下载  ·  点击「安装」运行安装包".to_string()
            } else {
                format!("{}  ·  进度已同步到下载管理", pct)
            };
            self.gfx.text(
                &caption,
                hint,
                11,
                false,
                if *state == DlState::Failed {
                    p.danger
                } else {
                    p.accent
                },
            );
        } else if let Some(info) = &local {
            let v = if info.version.is_empty() {
                String::new()
            } else {
                format!(" {}", info.version)
            };
            let text = if info.outdated {
                format!("本机已安装{}  ·  有新版本，点击右侧下载新版", v)
            } else {
                format!("本机已安装{}  ·  已是最新", v)
            };
            self.gfx.text(
                &text,
                hint,
                11,
                false,
                if info.outdated { p.accent } else { p.sub },
            );
        } else {
            self.gfx
                .text("获取后保存到 Downloads", hint, 11, false, p.sub);
        }
        let label = match task.as_ref().map(|t| t.0) {
            Some(DlState::Downloading | DlState::Queued) => "暂停",
            Some(DlState::Paused) => "继续下载",
            Some(DlState::Done) => "安装",
            Some(DlState::Failed | DlState::Canceled) => "重试",
            None => match &local {
                Some(info) if info.outdated => "获取新版",
                Some(info) if info.launch.is_some() => "打开",
                Some(_) => "已安装",
                None => "获取",
            },
        };
        draw_button(
            &mut self.gfx,
            p,
            Self::download_rect(w),
            label,
            task.is_some() || local.as_ref().is_none_or(|x| x.outdated),
            self.hover == Some((HIT_DOWNLOAD, 0)),
            false,
        );
        if local.is_some() {
            // Installed apps keep an explicit way to fetch the package again
            // (repair, offline installer, another machine) without pretending
            // the app is not installed.
            draw_button(
                &mut self.gfx,
                p,
                Self::reinstall_rect(w),
                "重新下载安装包",
                false,
                self.hover == Some((HIT_REINSTALL, 0)),
                false,
            );
        }
        if let Some((state, frac, _pct)) = task {
            downloads::progress_bar(
                &mut self.gfx,
                p,
                Self::download_progress_rect(w),
                frac,
                if state == DlState::Failed {
                    p.danger
                } else {
                    p.accent
                },
            );
        }
        self.gfx.fill(
            gfx::rect(
                NAV_W + PAD,
                DETAIL_DIVIDER_Y,
                w - PAD,
                DETAIL_DIVIDER_Y + 1.0,
            ),
            p.border,
        );

        // Scrolling body: heights come from the same measured layouts that are
        // drawn, so a long description pushes the following sections down
        // instead of painting on top of them.
        let layout = self.detail_layout(w);
        let x0 = NAV_W + PAD;
        let cw = Self::detail_text_width(w);
        let x1 = x0 + cw;
        let mut y = DETAIL_BODY_TOP - self.scroll as f32;

        self.section_title("应用简介", x0, y, x1);
        y += 34.0;
        let intro_r = gfx::rect(x0, y, x1, y + layout.intro_h + 2.0);
        self.gfx
            .paragraph(&layout.intro, intro_r, 13, false, p.text, 0);
        y += layout.intro_h + 22.0;

        if layout.whats_new_h > 0.0 {
            self.section_title("本次更新", x0, y, x1);
            y += 34.0;
            let wn_r = gfx::rect(x0, y, x1, y + layout.whats_new_h + 2.0);
            self.gfx
                .paragraph(&layout.whats_new, wn_r, 13, false, p.sub, 0);
            y += layout.whats_new_h + 22.0;
        }

        if !layout.info.is_empty() {
            self.section_title("应用信息", x0, y, x1);
            y += 34.0;
            for (k, v) in &layout.info {
                self.gfx
                    .text(k, gfx::rect(x0, y, x0 + 110.0, y + 26.0), 12, false, p.sub);
                self.gfx
                    .text(v, gfx::rect(x0 + 120.0, y, x1, y + 26.0), 12, false, p.text);
                self.gfx
                    .fill(gfx::rect(x0, y + 26.0, x1, y + 27.0), p.border);
                y += 28.0;
            }
        }
    }

    fn section_title(&mut self, label: &str, x0: f32, y: f32, x1: f32) {
        let p = self.gfx.pal;
        self.gfx
            .round(gfx::rect(x0, y + 3.0, x0 + 3.0, y + 21.0), 1.5, p.accent);
        self.gfx.text(
            label,
            gfx::rect(x0 + 14.0, y, x1, y + 26.0),
            15,
            true,
            p.text,
        );
    }

    // -----------------------------------------------------------------------
    // Hit testing: one source of truth for hover and click.
    // -----------------------------------------------------------------------

    fn hit_test(&self, x: f32, y: f32) -> Option<(u8, usize)> {
        let w = self.width as f32;
        let h = self.height as f32;
        if y < TOP_H {
            if in_rect(x, y, self.search_box_rect()) {
                return Some((HIT_SEARCH_BOX, 0));
            }
            if in_rect(x, y, Self::search_button_rect(w)) {
                return Some((HIT_SEARCH_BTN, 0));
            }
            return None;
        }
        if y > h - STATUS_H {
            if in_rect(x, y, Self::status_dl_rect(w, h)) || downloads::status_hit(self, x, y) {
                return Some((HIT_DL_NAV, 0));
            }
            return None;
        }
        if x < NAV_W {
            for i in 0..self.nav.len() {
                if in_rect(x, y, self.nav_rect(i)) {
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
                if in_rect(x, y, Self::download_rect(w)) {
                    return Some((HIT_DOWNLOAD, 0));
                }
                if in_rect(x, y, Self::reinstall_rect(w))
                    && self.apps.get(self.detail_index).is_some_and(|a| {
                        self.inventory
                            .status(&a.name, a.version.as_deref())
                            .is_some()
                    })
                {
                    return Some((HIT_REINSTALL, 0));
                }
                None
            }
            Mode::Updates => updates_ui::hit_targets(self, x, y),
            Mode::Downloads => downloads::hit_targets(self, x, y),
            Mode::Library => library::hit_targets(self, x, y),
            Mode::Browse => {
                if in_rect(x, y, Self::hero_dl_rect(w)) {
                    return Some((HIT_DL_NAV, 0));
                }
                for (i, (r, _)) in self.source_chip_rects(w).iter().enumerate() {
                    if in_rect(x, y, *r) {
                        return Some((HIT_SOURCE, i));
                    }
                }
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
        let into_field = target == Some((HIT_SEARCH_BOX, 0));
        self.focus_search = into_field;
        if into_field {
            self.search_set_caret_x(x);
        } else {
            self.blur_search();
        }
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
            Some((HIT_SOURCE, i)) => {
                let next = if i == 0 {
                    None
                } else {
                    self.sources.get(i - 1).cloned()
                };
                self.set_source_filter(next);
            }
            Some((HIT_DOWNLOAD, 0)) => {
                let i = self.detail_index;
                self.start_download(i, false);
            }
            Some((HIT_REINSTALL, 0)) => {
                let i = self.detail_index;
                self.start_download(i, true);
            }
            Some((HIT_INSTALL, i)) => self.start_download(i, false),
            Some((HIT_CARD, i)) => self.open_detail(i),
            Some((HIT_DL_NAV, _)) => {
                if let Some(i) = self.nav.iter().position(|n| n.kind == NavKind::Downloads) {
                    self.active_nav = i;
                }
                self.mode = Mode::Downloads;
                self.scroll = 0;
                self.focus_search = false;
                self.status = format!(
                    "下载管理  ·  {} 个任务  ·  {} 个进行中",
                    self.downloads.len(),
                    self.active_count()
                );
                self.invalidate();
            }
            Some((kind, i))
                if (HIT_DL_CANCEL..=HIT_DL_CLEAR).contains(&kind)
                    || matches!(kind, HIT_DL_INSTALL | HIT_DL_PAUSE) =>
            {
                downloads::apply(self, kind, i);
            }
            Some((kind, i))
                if kind == HIT_LIB_REFRESH
                    || (HIT_LIB_OPEN..=HIT_LIB_UNINSTALL).contains(&kind) =>
            {
                library::apply(self, kind, i);
            }
            Some((kind, i))
                if (HIT_CHECK..=HIT_ROW).contains(&kind)
                    || (HIT_IGNORED_LIST..=HIT_IGNORED_ALL).contains(&kind) =>
            {
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
            WM_SETCURSOR => {
                // Show an I-beam over the input field. Without this the arrow
                // never changes and the field does not feel typeable.
                if (lparam.0 as u32 & 0xFFFF) == HTCLIENT {
                    let mut pt = POINT::default();
                    unsafe {
                        let _ = GetCursorPos(&mut pt);
                        let _ = ScreenToClient(self.hwnd, &mut pt);
                    }
                    let s = self.gfx.scale.max(0.5);
                    let over = in_rect(pt.x as f32 / s, pt.y as f32 / s, self.search_box_rect());
                    let id = if over { IDC_IBEAM } else { IDC_ARROW };
                    unsafe {
                        if let Ok(c) = LoadCursorW(None, id) {
                            SetCursor(Some(c));
                        }
                    }
                    Some(LRESULT(1))
                } else {
                    None
                }
            }
            WM_TIMER => {
                if wparam.0 != TIMER_ID {
                    return None;
                }
                if self.focus_search {
                    self.caret_on = !self.caret_on;
                }
                self.sample_speeds();
                // Avoid repainting every other page twice a second.
                if self.mode == Mode::Downloads || self.focus_search {
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_KILLFOCUS => {
                self.caret_on = false;
                self.invalidate();
                None
            }
            WM_ACTIVATEAPP => {
                // The user may have installed or removed something in another
                // window. One throttled local scan refreshes both the update
                // badge and the "installed" labels on the store pages.
                if wparam.0 != 0 && self.last_local_scan.elapsed() >= Duration::from_secs(2) {
                    let changed = self.refresh_updates_local();
                    if changed && self.mode == Mode::Updates && !self.update_checking {
                        self.status = format!(
                            "本机版本已更新  ·  现有 {} 个可更新应用",
                            self.updates.len()
                        );
                    }
                    if changed {
                        self.invalidate();
                    }
                }
                None
            }
            WM_SIZE => {
                // Sizes here are physical pixels. `render()` derives the DIP
                // size from the client rect + current scale; writing physical
                // pixels into width/height made every hit-test wrong after a
                // resize until a later repaint happened to correct it.
                let cw = (lparam.0 as u32) & 0xFFFF;
                let ch = ((lparam.0 as u32) >> 16) & 0xFFFF;
                if cw > 0 && ch > 0 {
                    self.gfx.resize(cw, ch);
                }
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                let dpi = (wparam.0 & 0xFFFF) as f32;
                self.gfx.set_dpi(dpi.max(96.0), dpi.max(96.0));
                if lparam.0 != 0 {
                    let r = lparam.0 as *const RECT;
                    unsafe {
                        let _ = SetWindowPos(
                            self.hwnd,
                            None,
                            (*r).left,
                            (*r).top,
                            (*r).right - (*r).left,
                            (*r).bottom - (*r).top,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
                self.invalidate();
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
                    if (0xD800..=0xDBFF).contains(&c) {
                        self.high_surrogate = Some(c);
                    } else if (0xDC00..=0xDFFF).contains(&c) {
                        if let Some(hi) = self.high_surrogate.take() {
                            self.search_insert(&String::from_utf16_lossy(&[hi, c]));
                        }
                    } else if c >= 0x20 && c != 0x7F {
                        self.high_surrogate = None;
                        self.search_insert(&String::from_utf16_lossy(&[c]));
                    }
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                let key = wparam.0;
                let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
                if self.focus_search {
                    match key {
                        8 => self.search_backspace(),
                        46 => self.search_delete(),
                        37 => self.search_move(-1),
                        39 => self.search_move(1),
                        36 => self.search_move_edge(false),
                        35 => self.search_move_edge(true),
                        13 => self.do_search(),
                        65 if ctrl => {
                            self.search_select_all = true;
                            self.invalidate();
                        }
                        86 if ctrl => self.paste_text(),
                        _ => {}
                    }
                    return Some(LRESULT(0));
                }
                if key == 27 {
                    self.mode = Mode::Browse;
                    if self
                        .nav
                        .get(self.active_nav)
                        .is_some_and(|n| n.kind != NavKind::Browse)
                    {
                        self.active_nav = 0;
                    }
                    self.scroll = 0;
                    self.blur_search();
                    self.invalidate();
                    return Some(LRESULT(0));
                }
                None
            }
            WM_APP_DATA
            | WM_APP_LIST
            | WM_APP_FALLBACK
            | WM_APP_ICON
            | WM_APP_DETAIL
            | WM_APP_STATUS
            | WM_APP_DL_PROGRESS
            | WM_APP_DL_DONE
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
                // The catalog already probed and repaired every third-party
                // link on its worker thread, so what arrives here is usable
                // as-is (and still round-robin mixed).
                self.home = apps.clone();
                self.set_apps(apps, "推荐应用");
            }
            (WM_APP_DATA, Payload::Data(Err(e))) => {
                net::log(&format!("DATA err {}", e));
                self.loading = false;
                self.status = format!("连接失败：{}（检查网络后重启）", e);
                self.invalidate();
            }
            (WM_APP_FALLBACK, Payload::Fallback { id, url }) => {
                self.on_fallback(id, url);
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
                if !bytes.is_empty()
                    && let Some(bmp) = self.gfx.decode_icon(&bytes)
                {
                    self.gfx.put_icon(key, bmp);
                    self.invalidate();
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
            (
                WM_APP_DL_PROGRESS,
                Payload::DlProgress {
                    id,
                    done,
                    total,
                    speed,
                    disk,
                },
            ) => self.on_dl_progress(id, done, total, speed, disk),
            (WM_APP_DL_DONE, Payload::DlDone { id, result }) => self.on_dl_done(id, result),
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

/// Source filter chips ("全部源 / 联想 / 腾讯 / …") laid out inside the hero
/// banner. Kept as a free function so the geometry is unit-testable without a
/// window; the renderer and the hit-test both call it.
pub(crate) fn chip_label(sources: &[String], counts: &[usize], i: usize) -> String {
    if i == 0 {
        let total: usize = counts.iter().sum();
        return format!("全部源 {}", total);
    }
    match sources.get(i - 1) {
        Some(name) => {
            let n = counts.get(i - 1).copied().unwrap_or(0);
            format!("{} {}", name, n)
        }
        None => String::new(),
    }
}

pub(crate) fn source_chips(
    w: f32,
    sources: &[String],
    counts: &[usize],
) -> Vec<(D2D_RECT_F, Option<String>)> {
    let hero = gfx::rect(NAV_W + PAD, TOP_H + 14.0, w - PAD, TOP_H + 14.0 + HERO_H);
    let btn = AppState::hero_dl_rect(w);
    // The chips own the whole lower band: everything left of the button.
    let right = btn.left - 16.0;
    let mut out = Vec::new();
    if right < hero.left + 120.0 {
        return out;
    }
    let mut x = hero.left + 24.0;
    // Starts below the 19 px title band (top+12..top+48) so the two never
    // touch, and stays inside the 88 px hero card.
    let y0 = hero.top + 48.0;
    let y1 = y0 + 26.0;
    let mut items: Vec<Option<String>> = vec![None];
    items.extend(sources.iter().take(6).cloned().map(Some));
    for (i, it) in items.iter().enumerate() {
        // The chip carries its own count: "联想 341" tells the user what the
        // filter is worth before they even click it.
        let label = chip_label(sources, counts, i);
        // Wide enough for 11 px bold text: CJK glyphs are ~1 em, ASCII ~0.6.
        // Padding stays tight so three sources plus "全部源" fit even at the
        // minimum window width.
        let text_w: f32 = label
            .chars()
            .map(|c| if c.is_ascii() { 6.6 } else { 11.2 })
            .sum();
        let cw = (text_w + 20.0).max(56.0);
        if x + cw > right {
            break;
        }
        out.push((gfx::rect(x, y0, x + cw, y1), it.clone()));
        x += cw + 8.0;
    }
    out
}

/// Kick off the initial network load on a worker thread.
pub fn start_load(hwnd_usize: usize) {
    std::thread::spawn(move || {
        let result = (|| -> Result<(String, Vec<ClassGroup>, Vec<App>), String> {
            let token = net::register()?;
            let classes = net::classes(&token)?;
            // One merged list from every enabled source; the sidebar still
            // navigates Lenovo categories, the landing page shows everything.
            let mut progress = |d: usize, t: usize| {
                post(
                    hwnd_usize,
                    WM_APP_STATUS,
                    Payload::Status(format!("正在聚合软件源 {}/{}…", d, t)),
                );
            };
            let mut note = |text: &str| {
                post(hwnd_usize, WM_APP_STATUS, Payload::Status(text.to_string()));
            };
            let merged = crate::source::catalog(&token, &mut progress, &mut note);
            if merged.apps.is_empty() {
                return Err(if merged.warnings.is_empty() {
                    "无法获取应用目录".to_string()
                } else {
                    merged.warnings.join("；")
                });
            }
            Ok((token, classes, merged.apps))
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

    /// Every card text band is disjoint from every other band and from the
    /// action button — the renderer and these tests use the same helpers.
    #[test]
    fn card_bands_never_overlap() {
        for cw in [250.0f32, CARD_W, 420.0] {
            let card = (0.0, 0.0, cw, CARD_H);
            let install = AppState::install_rect(card);
            let bands = [
                ("name", AppState::card_name_rect(card)),
                ("description", AppState::card_desc_rect(card)),
                ("meta chips", AppState::card_meta_rect(card)),
                ("progress bar", AppState::card_bar_rect(card)),
                ("progress caption", AppState::card_status_rect(card)),
            ];
            for (i, (label, r)) in bands.iter().enumerate() {
                assert!(r.right > r.left, "cw {cw}: {label} has no width");
                assert!(
                    !overlaps(*r, install),
                    "cw {cw}: {label} overlaps install button"
                );
                for (other_label, other) in bands.iter().take(i) {
                    assert!(
                        !overlaps(*r, *other),
                        "cw {cw}: {label} overlaps {other_label}"
                    );
                }
            }
            assert!(install.bottom <= CARD_H);
            assert!(bands[4].1.bottom <= CARD_H);
        }
    }

    /// The 0.4.0 detail page drew a full-width progress bar through the
    /// caption text. All hero bands must now be vertically separated.
    #[test]
    fn detail_hero_bands_never_overlap() {
        let w = 1100.0f32;
        let button = AppState::download_rect(w);
        let caption = AppState::detail_progress_text_rect(w);
        let bar = AppState::download_progress_rect(w);
        // The caption is a full-width band below the action button now.
        assert!(!overlaps(caption, button), "caption crosses action button");
        assert!(!overlaps(caption, bar), "caption overlaps progress bar");
        assert!(!overlaps(button, bar), "button overlaps progress bar");
        assert!(bar.bottom < DETAIL_HERO_BOTTOM, "bar leaves the hero card");
        assert!(AppState::back_rect().bottom < DETAIL_HERO_BOTTOM);
    }

    #[test]
    fn detail_body_measurement_matches_drawing_width() {
        // A mismatch here would make DirectWrite wrap into an extra line that
        // the reserved height did not account for -> overlapping headings.
        for w in [1000.0f32, 1100.0, 1600.0] {
            let cw = AppState::detail_text_width(w);
            let x0 = NAV_W + PAD;
            let drawn_right = (w - PAD - 10.0).max(x0 + 180.0);
            assert!((x0 + cw - drawn_right).abs() < 0.01, "w={w}");
        }
    }

    #[test]
    fn header_search_and_button_do_not_overlap() {
        let w = 1100.0f32;
        let box_r = (w - 116.0).max(392.0);
        let search = gfx::rect(252.0, 18.0, box_r, 54.0);
        let button = AppState::search_button_rect(w);
        assert!(!overlaps(search, button));
        assert!(button.right <= w - 16.0);
    }

    #[test]
    fn banner_columns_are_disjoint() {
        let w = 1100.0f32;
        let hero = gfx::rect(NAV_W + PAD, TOP_H + 14.0, w - PAD, TOP_H + 14.0 + HERO_H);
        let btn = AppState::hero_dl_rect(w);
        let stat_right = btn.left - 20.0;
        let stat_left = stat_right - 236.0;
        let text_right = stat_left - 16.0;
        assert!(hero.left + 24.0 < text_right);
        assert!(text_right < stat_left);
        assert!(stat_left < stat_right);
        assert!(stat_right < btn.left);
        assert!(btn.right <= hero.right);
        assert!(btn.bottom <= hero.bottom);
        assert!(banner_bottom_for_test(hero) < TOP_H + HERO_H + 28.0);
    }

    fn banner_bottom_for_test(hero: D2D_RECT_F) -> f32 {
        hero.bottom
    }

    /// Every chip states how many rows it holds, so "全部源" and a single
    /// source can never look like the same button.
    #[test]
    fn chips_report_their_own_counts() {
        let sources = vec!["联想".to_string(), "腾讯".to_string()];
        let counts = vec![341usize, 395];
        assert_eq!(chip_label(&sources, &counts, 0), "全部源 736");
        assert_eq!(chip_label(&sources, &counts, 1), "联想 341");
        assert_eq!(chip_label(&sources, &counts, 2), "腾讯 395");
        assert_eq!(chip_label(&sources, &counts, 3), "");
    }

    /// The source chips replaced the tagline; they must stay inside the hero,
    /// clear of the stats column, and never overlap each other.
    #[test]
    fn source_chips_stay_inside_the_banner() {
        let sources = vec![
            "联想".to_string(),
            "腾讯".to_string(),
            "我的内网源".to_string(),
        ];
        for w in [760.0f32, 900.0, 1100.0, 1600.0] {
            let hero = gfx::rect(NAV_W + PAD, TOP_H + 14.0, w - PAD, TOP_H + 14.0 + HERO_H);
            let btn = AppState::hero_dl_rect(w);
            // Chips run all the way to the download button, so three typical
            // sources must fit even at the minimum window width.
            let text_right = btn.left - 16.0;
            let counts = vec![341usize, 395, 12];
            let chips = source_chips(w, &sources, &counts);
            if chips.is_empty() {
                // Only acceptable when there is genuinely no room left.
                assert!(text_right < hero.left + 120.0, "w={w}: chips dropped early");
                continue;
            }
            assert_eq!(
                chips[0].0.left,
                hero.left + 24.0,
                "w={w}: first chip offset"
            );
            assert_eq!(chips.len(), 4, "w={w}: every source needs a chip");
            for (i, (r, _)) in chips.iter().enumerate() {
                assert!(r.right > r.left, "w={w}: chip {i} has no width");
                assert!(r.top > hero.top + 40.0, "w={w}: chip {i} hits the title");
                assert!(r.bottom < hero.bottom, "w={w}: chip {i} leaves the hero");
                assert!(r.right <= text_right, "w={w}: chip {i} enters the stats");
                if i > 0 {
                    assert!(r.left > chips[i - 1].0.right, "w={w}: chips {i} overlap");
                }
            }
        }
    }

    #[test]
    fn banner_leaves_room_for_the_first_card_row() {
        let banner_bottom = TOP_H + 14.0 + HERO_H;
        assert!(banner_bottom < TOP_H + HERO_H + 28.0);
        assert!(TOP_H + HERO_H + 28.0 + CARD_H < MIN_H as f32);
    }

    #[test]
    fn nav_footer_is_reserved_below_the_last_row() {
        let h = 620.0f32;
        let footer_top = h - STATUS_H - 40.0;
        let version = gfx::rect(20.0, footer_top + 4.0, NAV_W - 16.0, h - STATUS_H - 6.0);
        assert!(version.bottom < h - STATUS_H);
        assert!(footer_top > TOP_H);
    }

    /// Regression for the screenshot that showed one card and a huge empty
    /// gutter on a 125%-DPI 1100 px window.
    #[test]
    fn columns_fill_the_pane_and_never_leave_a_lone_card() {
        for w in [760.0f32, 860.0, 1100.0, 1600.0] {
            let (cols, cw) = layout_columns(w);
            assert!(cols >= 2, "width {w} produced a single column");
            let content = w - NAV_W - 2.0 * PAD;
            let used = cols as f32 * cw + (cols - 1) as f32 * GAP;
            assert!(
                (used - content.max(240.0)).abs() < 0.01,
                "width {w}: cards do not fill the pane"
            );
            assert!(cw > 200.0, "width {w}: cards became too narrow");
        }
    }

    #[test]
    fn stretched_narrow_cards_keep_their_action_column() {
        let cw = 250.0f32;
        let card = (0.0, 0.0, cw, CARD_H);
        let install = AppState::install_rect(card);
        let bar = AppState::card_bar_rect(card);
        let status = AppState::card_status_rect(card);
        assert!(bar.right > bar.left && status.right > status.left);
        assert!(!overlaps(bar, install));
        assert!(!overlaps(status, install));
        assert!(install.right <= cw - 10.0);
    }

    /// Regression for the download row's percentage: the old rect had
    /// `right < left`, so the number was never painted.
    #[test]
    fn download_row_percentage_has_a_real_rectangle() {
        let r = gfx::rect(NAV_W + PAD, TOP_H + 110.0, 1600.0 - PAD, TOP_H + 210.0);
        let text_right = r.right - 276.0;
        let pct = gfx::rect(
            text_right + 10.0,
            r.top + 64.0,
            text_right + 64.0,
            r.top + 88.0,
        );
        assert!(pct.right > pct.left, "percentage rect is inverted");
        assert!(pct.right - pct.left >= 40.0);
        let (primary, secondary, tertiary) = AppState::dl_buttons(r);
        assert!(!overlaps(pct, primary));
        assert!(!overlaps(pct, secondary));
        assert!(!overlaps(pct, tertiary));
    }

    #[test]
    fn status_bar_zones_do_not_overlap() {
        let w = 1100.0f32;
        let status = gfx::rect(16.0, 0.0, (w - 430.0).max(160.0), STATUS_H);
        let progress_left = (w - 400.0).max(200.0);
        let download = AppState::status_dl_rect(w, STATUS_H);
        assert!(status.right < progress_left);
        assert!(progress_left + 120.0 < w - 196.0);
        assert!(
            w - 196.0 < download.left,
            "progress overlaps download button"
        );
    }

    #[test]
    fn search_field_is_a_real_rect_and_leaves_the_title_alone() {
        for w in [1000.0f32, 1100.0, 2048.0] {
            // The renderer derives the rect from self.width; the geometry is
            // the same formula, so assert the token relationship directly.
            let r = gfx::rect(252.0, 18.0, (w - 116.0).max(392.0), 54.0);
            assert!(r.right > r.left + 100.0);
            assert!(r.top >= 0.0 && r.bottom <= TOP_H);
            let title = gfx::rect(66.0, 12.0, 240.0, 62.0);
            assert!(!overlaps(r, title), "search field covers the app title");
            assert!(r.left > title.right);
            let button = AppState::search_button_rect(w);
            assert!(r.right < button.left, "field overlaps search button");
        }
    }

    /// 0.7.0: a third header button ("ignored updates") must not collide with
    /// the title, the check button or the update-all button at any width.
    #[test]
    fn update_header_buttons_are_disjoint() {
        for w in [1000.0f32, 1100.0, 1600.0] {
            let ignored = AppState::ignored_button_rect(w);
            let check = AppState::check_button_rect(w);
            let all = AppState::all_button_rect(w);
            let title = gfx::rect(NAV_W + PAD, TOP_H + 12.0, w - 420.0, TOP_H + 44.0);
            assert!(title.right < ignored.left, "w={w}: title crosses ignored");
            assert!(ignored.right < check.left, "w={w}: ignored crosses check");
            assert!(check.right < all.left, "w={w}: check crosses all");
            assert!(all.right <= w - PAD + 0.01);
            assert!(!overlaps(title, ignored));
            assert!(!overlaps(ignored, check));
            assert!(!overlaps(check, all));
        }
    }

    #[test]
    fn ignored_row_restore_button_leaves_text_room() {
        let r = gfx::rect(
            NAV_W + PAD,
            TOP_H + 84.0,
            1100.0 - PAD,
            TOP_H + 84.0 + ROW_H,
        );
        let (primary, secondary) = AppState::row_buttons(r);
        assert!(primary.right <= r.right - 10.0);
        assert!(secondary.right < primary.left);
        let text = gfx::rect(
            r.left + 84.0,
            r.top + 12.0,
            primary.left - 12.0,
            r.top + 40.0,
        );
        assert!(text.right > text.left + 100.0);
        assert!(!overlaps(text, primary));
    }

    /// The "re-download" action for installed apps has its own band: it must
    /// not paint over the status caption, the primary action or the progress
    /// bar (the 0.4.0 overlap class of bugs).
    #[test]
    fn reinstall_button_owns_its_own_band() {
        for w in [1000.0f32, 1100.0, 1600.0] {
            let r = AppState::reinstall_rect(w);
            let caption = AppState::detail_progress_text_rect(w);
            let action = AppState::download_rect(w);
            let bar = AppState::download_progress_rect(w);
            let meta = gfx::rect(
                NAV_W + PAD + 140.0,
                DETAIL_HERO_TOP + 92.0,
                w - PAD - 214.0,
                DETAIL_HERO_TOP + 140.0,
            );
            assert!(!overlaps(r, action), "w={w}: crosses primary action");
            assert!(!overlaps(r, caption), "w={w}: crosses caption");
            assert!(!overlaps(r, meta), "w={w}: crosses metadata text");
            assert!(!overlaps(r, bar), "w={w}: crosses progress bar");
            assert!(
                caption.right > caption.left + 300.0,
                "w={w}: caption too narrow"
            );
            assert!(r.bottom < DETAIL_HERO_BOTTOM);
        }
    }

    #[test]
    fn caret_moves_by_whole_utf16_characters() {
        // "A" + emoji (surrogate pair) + "B"
        let units: Vec<u16> = "A😀B".encode_utf16().collect();
        assert_eq!(units.len(), 4);
        let end = units.len();
        assert_eq!(AppState::prev_boundary(&units, end), 3);
        assert_eq!(AppState::prev_boundary(&units, 3), 1);
        assert_eq!(AppState::prev_boundary(&units, 1), 0);
        assert_eq!(AppState::prev_boundary(&units, 0), 0);
        assert_eq!(AppState::next_boundary(&units, 0), 1);
        assert_eq!(AppState::next_boundary(&units, 1), 3);
        assert_eq!(AppState::next_boundary(&units, 3), 4);
        assert_eq!(AppState::next_boundary(&units, 4), 4);
    }
}
