//! The settings page: which catalog sources are queried, user-added sources,
//! the download folder, how many transfers may run at once, and the theme.
//!
//! Everything is saved to `%LOCALAPPDATA%\AllStore\sources.json` the moment it
//! changes — there is no "apply" button and no server round trip.

use std::ffi::c_void;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
};

use crate::app::{
    AppState, BTN_H, GAP, HIT_SET_ADD, HIT_SET_CONCURRENCY, HIT_SET_DIR, HIT_SET_DIR_RESET,
    HIT_SET_FIELD, HIT_SET_REMOVE, HIT_SET_SELF_CHECK, HIT_SET_SELF_GET, HIT_SET_THEME,
    HIT_SET_TOGGLE, NAV_W, PAD, STATUS_H, TOP_H, WM_APP_SELFUPDATE, draw_button, in_rect, post,
};
use crate::downloads;
use crate::gfx;
use crate::source::{self, Theme};

/// Built-in feeds in the order they are listed. The label comes from the
/// settings file's display name, which is exactly what `disabled` stores.
const BUILTIN: [(&str, &str); 3] = [
    ("联想应用商店", source::LENOVO),
    ("腾讯软件中心", source::TENCENT),
    ("GitHub 开源软件", source::GITHUB),
];

const HEADER_H: f32 = 30.0;
const ROW: f32 = 54.0;
const CARD_PAD: f32 = 8.0;
const SECTION_GAP: f32 = 26.0;
/// Single horizontal inset shared by every card, so labels, buttons and fields
/// line up on one vertical grid instead of drifting 16/20 px apart.
const INSET: f32 = 20.0;
/// Width of the label column in a two-column row ("下载目录", "同时下载"…).
const LABEL_W: f32 = 150.0;
/// Gap between the label column and the control column.
const COL_GAP: f32 = 20.0;
const CONCURRENCY: [usize; 4] = [1, 2, 3, 5];
const THEME_LABELS: [&str; 3] = ["跟随系统", "浅色", "深色"];
/// "跟随系统" is the only three-glyph label, so it gets its own width; the
/// other two share the narrow one used by the concurrency chips.
const THEME_W: [f32; 3] = [84.0, 64.0, 64.0];
// A theme chip without a label used to index past the end of the label array.
const _: () = assert!(THEME_LABELS.len() == THEME_W.len());

struct Layout {
    x0: f32,
    x1: f32,
    src_card: D2D_RECT_F,
    toggle_rows: Vec<D2D_RECT_F>,
    custom_card: D2D_RECT_F,
    add_name: D2D_RECT_F,
    add_url: D2D_RECT_F,
    add_btn: D2D_RECT_F,
    custom_rows: Vec<D2D_RECT_F>,
    custom_remove: Vec<D2D_RECT_F>,
    dir_card: D2D_RECT_F,
    dir_btn: D2D_RECT_F,
    dir_reset: D2D_RECT_F,
    conc_chips: Vec<D2D_RECT_F>,
    theme_chips: Vec<D2D_RECT_F>,
    about_card: D2D_RECT_F,
    about_check: D2D_RECT_F,
    about_get: D2D_RECT_F,
    /// Absolute y of the first section header, used by the painter.
    src_header_y: f32,
    custom_header_y: f32,
    dir_header_y: f32,
    about_header_y: f32,
    bottom: f32,
}

fn chip(x: f32, y: f32, w: f32) -> D2D_RECT_F {
    gfx::rect(x, y, x + w, y + 30.0)
}

fn layout(st: &AppState) -> Layout {
    layout_at(st.width as f32, st.scroll as f32, st.settings.custom.len())
}

/// Pure geometry, split out from `AppState` so the alignment rules below can be
/// asserted in unit tests without a window or a Direct2D device.
fn layout_at(w: f32, scroll: f32, custom: usize) -> Layout {
    let x0 = NAV_W + PAD;
    let x1 = (w - PAD).max(x0 + 340.0);
    // Title + subtitle occupy the top of the page.
    let mut y = TOP_H + 78.0 - scroll;

    let src_header_y = y;
    y += HEADER_H;
    let src_card = gfx::rect(x0, y, x1, y + CARD_PAD * 2.0 + ROW * 3.0);
    let toggle_rows: Vec<D2D_RECT_F> = (0..3)
        .map(|i| {
            gfx::rect(
                x0,
                src_card.top + CARD_PAD + ROW * i as f32,
                x1,
                src_card.top + CARD_PAD + ROW * (i as f32 + 1.0),
            )
        })
        .collect();
    y = src_card.bottom + SECTION_GAP;

    let custom_header_y = y;
    y += HEADER_H;
    let add_top = y + CARD_PAD + 8.0;
    let add_bottom = add_top + 36.0;
    let add_left = x0 + INSET;
    let add_right = x1 - INSET;
    let add_btn = gfx::rect(add_right - 88.0, add_top, add_right, add_bottom);
    let add_name = gfx::rect(add_left, add_top, add_left + 200.0, add_bottom);
    let add_url = gfx::rect(
        add_name.right + 10.0,
        add_top,
        add_btn.left - 10.0,
        add_bottom,
    );
    let custom_n = custom as f32;
    // The empty state needs one more line under the fields for the hint; the
    // hint used to be drawn on top of the URL field.
    let custom_body = if custom_n > 0.0 {
        8.0 + custom_n * 46.0
    } else {
        34.0
    };
    let custom_card = gfx::rect(x0, y, x1, y + CARD_PAD + 52.0 + custom_body + CARD_PAD);
    let first = custom_card.top + CARD_PAD + 52.0 + 8.0;
    let custom_rows: Vec<D2D_RECT_F> = (0..custom)
        .map(|i| {
            gfx::rect(
                x0,
                first + 46.0 * i as f32,
                x1,
                first + 46.0 * (i as f32 + 1.0),
            )
        })
        .collect();
    let custom_remove: Vec<D2D_RECT_F> = custom_rows
        .iter()
        .map(|r| {
            gfx::rect(
                r.right - INSET - 76.0,
                r.top + 7.0,
                r.right - INSET,
                r.top + 7.0 + BTN_H,
            )
        })
        .collect();
    y = custom_card.bottom + SECTION_GAP;

    let dir_header_y = y;
    y += HEADER_H;
    let dir_card = gfx::rect(x0, y, x1, y + CARD_PAD * 2.0 + ROW * 3.0);
    let dir_row = gfx::rect(
        x0,
        dir_card.top + CARD_PAD,
        x1,
        dir_card.top + CARD_PAD + ROW,
    );
    let dir_reset = gfx::rect(
        x1 - INSET - 96.0,
        dir_row.top + (ROW - BTN_H) * 0.5,
        x1 - INSET,
        dir_row.top + (ROW + BTN_H) * 0.5,
    );
    let dir_btn = gfx::rect(
        dir_reset.left - 8.0 - 88.0,
        dir_reset.top,
        dir_reset.left - 8.0,
        dir_reset.bottom,
    );
    let conc_row = gfx::rect(x0, dir_row.bottom, x1, dir_row.bottom + ROW);
    let theme_row = gfx::rect(x0, conc_row.bottom, x1, conc_row.bottom + ROW);
    let chip_y = conc_row.top + (ROW - 30.0) * 0.5;
    // One control column for every two-column row, including the chip rows.
    let control_left = |base: &D2D_RECT_F| base.left + INSET + LABEL_W + COL_GAP;
    let chips = |base: &D2D_RECT_F| -> Vec<D2D_RECT_F> {
        (0..4)
            .map(|i| {
                chip(
                    control_left(base) + i as f32 * 76.0,
                    chip_y.max(base.top + 12.0),
                    68.0,
                )
            })
            .collect()
    };
    let conc_chips = chips(&conc_row);
    let mut chip_x = control_left(&theme_row);
    let theme_chips: Vec<D2D_RECT_F> = THEME_W
        .iter()
        .map(|w| {
            let r = chip(chip_x, chip_y.max(theme_row.top + 12.0), *w);
            chip_x = r.right + 8.0;
            r
        })
        .collect();

    let about_header_y = dir_card.bottom + SECTION_GAP;
    y = about_header_y + HEADER_H;
    let about_card = gfx::rect(x0, y, x1, y + 96.0);
    let about_get = gfx::rect(
        about_card.right - INSET - 104.0,
        about_card.top + 32.0,
        about_card.right - INSET,
        about_card.top + 32.0 + BTN_H,
    );
    let about_check = gfx::rect(
        about_get.left - 8.0 - 92.0,
        about_get.top,
        about_get.left - 8.0,
        about_get.bottom,
    );

    Layout {
        x0,
        x1,
        src_card,
        toggle_rows,
        custom_card,
        add_name,
        add_url,
        add_btn,
        custom_rows,
        custom_remove,
        dir_card,
        dir_btn,
        dir_reset,
        conc_chips,
        theme_chips,
        about_card,
        about_check,
        about_get,
        src_header_y,
        custom_header_y,
        dir_header_y,
        about_header_y,
        bottom: about_card.bottom + 24.0,
    }
}

pub(crate) fn max_scroll(st: &AppState, _h: f32) -> i32 {
    let l = layout(st);
    // `layout` already subtracted the current scroll offset; add it back so the
    // total height is independent of how far the page is scrolled.
    let total = l.bottom + st.scroll as f32 - (TOP_H + 12.0);
    let visible = (st.height as f32 - STATUS_H - TOP_H - 12.0).max(0.0);
    (total - visible).max(0.0) as i32
}

pub(crate) fn hit_targets(st: &AppState, x: f32, y: f32) -> Option<(u8, usize)> {
    let l = layout(st);
    for (i, r) in l.toggle_rows.iter().enumerate() {
        if in_rect(x, y, *r) {
            return Some((HIT_SET_TOGGLE, i));
        }
    }
    if in_rect(x, y, l.add_name) {
        return Some((HIT_SET_FIELD, 0));
    }
    if in_rect(x, y, l.add_url) {
        return Some((HIT_SET_FIELD, 1));
    }
    if in_rect(x, y, l.add_btn) {
        return Some((HIT_SET_ADD, 0));
    }
    for (i, r) in l.custom_remove.iter().enumerate() {
        if in_rect(x, y, *r) {
            return Some((HIT_SET_REMOVE, i));
        }
    }
    if in_rect(x, y, l.dir_btn) {
        return Some((HIT_SET_DIR, 0));
    }
    if in_rect(x, y, l.dir_reset) {
        return Some((HIT_SET_DIR_RESET, 0));
    }
    for (i, r) in l.conc_chips.iter().enumerate() {
        if in_rect(x, y, *r) {
            return Some((HIT_SET_CONCURRENCY, CONCURRENCY[i]));
        }
    }
    for (i, r) in l.theme_chips.iter().enumerate() {
        if in_rect(x, y, *r) {
            return Some((HIT_SET_THEME, i));
        }
    }
    if in_rect(x, y, l.about_check) {
        return Some((HIT_SET_SELF_CHECK, 0));
    }
    if in_rect(x, y, l.about_get) && st.self_update.is_some() {
        return Some((HIT_SET_SELF_GET, 0));
    }
    None
}

fn enabled(st: &AppState, name: &str) -> bool {
    !st.settings.disabled.iter().any(|d| d == name)
}

fn set_enabled(st: &mut AppState, name: &str, on: bool) {
    st.settings.disabled.retain(|d| d != name);
    if !on {
        st.settings.disabled.push(name.to_string());
    }
}

fn persist(st: &mut AppState) {
    match source::save_settings(&st.settings) {
        Ok(()) => st.status = "设置已保存到本机".into(),
        Err(e) => st.status = e,
    }
}

/// Load the catalog again so a source change is visible immediately instead of
/// on the next launch. Only called after the source list itself changed.
fn reload_catalog(st: &mut AppState) {
    st.status = "设置已保存  ·  正在重新聚合软件源…".into();
    crate::app::start_load(st.hwnd_usize());
}

/// Add the typed custom source. Enter in either field calls this too.
pub(crate) fn commit_custom(st: &mut AppState) {
    let name = String::from_utf16_lossy(&st.src_name).trim().to_string();
    let url = String::from_utf16_lossy(&st.src_url).trim().to_string();
    if name.is_empty() || url.is_empty() {
        st.status = "请填写源名称和 JSON 地址".into();
        st.invalidate();
        return;
    }
    if !url.starts_with("https://") && !url.starts_with("http://") {
        st.status = "地址必须以 http:// 或 https:// 开头".into();
        st.invalidate();
        return;
    }
    if BUILTIN.iter().any(|(_, n)| *n == name) || st.settings.custom.iter().any(|c| c.name == name)
    {
        st.status = format!("已存在名为「{}」的源", name);
        st.invalidate();
        return;
    }
    if st.settings.custom.len() >= 8 {
        st.status = "自定义源最多 8 个".into();
        st.invalidate();
        return;
    }
    st.settings.custom.push(source::CustomSource {
        name: name.clone(),
        url,
    });
    st.src_name.clear();
    st.src_url.clear();
    st.field_caret = 0;
    st.field = Some(crate::app::Field::SourceName);
    persist(st);
    reload_catalog(st);
    st.invalidate();
}

pub(crate) fn apply(st: &mut AppState, kind: u8, idx: usize) {
    match kind {
        HIT_SET_TOGGLE => {
            let Some((label, name)) = BUILTIN.get(idx) else {
                return;
            };
            let on = !enabled(st, name);
            set_enabled(st, name, on);
            st.status = format!(
                "{} 已{}",
                label,
                if on {
                    "启用，正在重新聚合…"
                } else {
                    "停用"
                }
            );
            persist(st);
            if on {
                reload_catalog(st);
            } else {
                st.invalidate();
            }
        }
        HIT_SET_ADD => commit_custom(st),
        HIT_SET_REMOVE => {
            if idx < st.settings.custom.len() {
                let removed = st.settings.custom.remove(idx);
                persist(st);
                reload_catalog(st);
                st.status = format!("已移除自定义源「{}」", removed.name);
            }
        }
        HIT_SET_DIR => {
            if let Some(dir) = pick_folder(st.hwnd) {
                st.settings.download_dir = dir;
                persist(st);
                st.invalidate();
            }
        }
        HIT_SET_DIR_RESET => {
            st.settings.download_dir.clear();
            persist(st);
            st.invalidate();
        }
        HIT_SET_CONCURRENCY => {
            st.settings.max_concurrent = idx;
            downloads::set_max_concurrent(idx);
            persist(st);
            st.invalidate();
        }
        HIT_SET_THEME => {
            st.settings.theme = match idx {
                1 => Theme::Light,
                2 => Theme::Dark,
                _ => Theme::System,
            };
            st.apply_theme();
            persist(st);
            st.invalidate();
        }
        HIT_SET_SELF_CHECK => st.check_self_update(),
        HIT_SET_SELF_GET => {
            let Some(rel) = st.self_update.clone() else {
                return;
            };
            match rel.asset {
                Some(url) => {
                    let name = format!("AllStore {}", rel.version);
                    let id = st.new_download(&name, &url, None);
                    st.start_task(id);
                    st.status = format!("已把 AllStore {} 加入下载队列", rel.version);
                    st.mode = crate::app::Mode::Downloads;
                }
                None => crate::selfupdate::open_url(&rel.page),
            }
            st.invalidate();
        }
        _ => {}
    }
}

/// Native folder picker. Returns `None` when the user cancels.
fn pick_folder(owner: HWND) -> Option<String> {
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
        let opts = dialog.GetOptions().ok()?;
        dialog
            .SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .ok()?;
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let text = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const c_void));
        text
    }
}

impl AppState {
    /// Ask GitHub for the latest release in the background. Cheap, read-only,
    /// and started once per launch; the manual button reuses the same path.
    pub(crate) fn check_self_update(&mut self) {
        if self.checking_update {
            return;
        }
        self.checking_update = true;
        self.self_update_note = "正在检查更新…".into();
        let hwnd = self.hwnd_usize();
        std::thread::spawn(move || {
            post(
                hwnd,
                WM_APP_SELFUPDATE,
                crate::app::Payload::SelfUpdate(crate::selfupdate::check()),
            );
        });
        self.invalidate();
    }

    pub(crate) fn on_self_update(
        &mut self,
        result: Result<Option<crate::selfupdate::Release>, String>,
    ) {
        self.checking_update = false;
        match result {
            Ok(Some(rel)) => {
                self.self_update_note = format!("发现新版本 {}，可立即下载", rel.version);
                self.status = format!("AllStore 有新版本 {}，可在「设置」中查看", rel.version);
                self.self_update = Some(rel);
            }
            Ok(None) => {
                self.self_update = None;
                self.self_update_note =
                    format!("已是最新版本（v{}）", crate::selfupdate::current());
            }
            Err(e) => {
                // Offline is the normal case on a locked-down network; keep the
                // message factual instead of alarming.
                self.self_update = None;
                self.self_update_note = format!("暂时无法检查更新：{}", e);
            }
        }
        self.invalidate();
    }
}

impl AppState {
    pub(crate) fn render_settings(&mut self, w: f32, _h: f32) {
        let p = self.gfx.pal;
        let l = layout(self);
        let x0 = l.x0;

        self.gfx.text(
            "设置",
            gfx::rect(x0, TOP_H + 10.0, w - 240.0, TOP_H + 40.0),
            19,
            true,
            p.text,
        );
        self.gfx.text(
            "全部为本机设置，立即保存，不联网同步",
            gfx::rect(x0, TOP_H + 42.0, w - 240.0, TOP_H + 66.0),
            11,
            false,
            p.sub,
        );

        // --- source toggles ---------------------------------------------------
        self.gfx.text(
            "内容源",
            gfx::rect(x0, l.src_header_y, l.x1, l.src_header_y + 26.0),
            12,
            true,
            p.sub,
        );
        self.gfx.round(l.src_card, 14.0, p.card);
        self.gfx.outline(l.src_card, p.border, 0.8);
        for (i, row) in l.toggle_rows.iter().enumerate() {
            let name = BUILTIN[i].1;
            let on = enabled(self, name);
            if i > 0 {
                self.gfx.line(
                    row.left + INSET,
                    row.top,
                    row.right - INSET,
                    row.top,
                    p.border,
                    0.6,
                );
            }
            self.gfx.text(
                BUILTIN[i].0,
                gfx::rect(row.left + INSET, row.top, row.right - 160.0, row.bottom),
                13,
                true,
                p.text,
            );
            let pill = gfx::rect(
                row.right - INSET - 72.0,
                row.top + (ROW - 28.0) * 0.5,
                row.right - INSET,
                row.top + (ROW + 28.0) * 0.5,
            );
            let hot = self.hover == Some((HIT_SET_TOGGLE, i));
            self.gfx.round(
                pill,
                14.0,
                if on {
                    p.accent
                } else if hot {
                    p.hover
                } else {
                    p.subtle
                },
            );
            self.gfx.center_text(
                if on { "已启用" } else { "已停用" },
                pill,
                11,
                true,
                // Off is still clickable, so it keeps body colour like the
                // unselected chips; only the fill tells the two states apart.
                if on { p.on_accent } else { p.text },
            );
        }

        // --- custom sources ---------------------------------------------------
        self.gfx.text(
            "自定义源",
            gfx::rect(x0, l.custom_header_y, l.x1, l.custom_header_y + 26.0),
            12,
            true,
            p.sub,
        );
        self.gfx.round(l.custom_card, 14.0, p.card);
        self.gfx.outline(l.custom_card, p.border, 0.8);
        self.draw_field(
            l.add_name,
            "名称",
            &self.src_name.clone(),
            self.field == Some(crate::app::Field::SourceName),
        );
        self.draw_field(
            l.add_url,
            "JSON 地址",
            &self.src_url.clone(),
            self.field == Some(crate::app::Field::SourceUrl),
        );
        draw_button(
            &mut self.gfx,
            p,
            l.add_btn,
            "添加",
            true,
            self.hover == Some((HIT_SET_ADD, 0)),
            false,
        );
        if self.settings.custom.is_empty() {
            self.gfx.text(
                "可选：填写返回联想目录格式 JSON 的地址，添加后与内置源一起聚合",
                gfx::rect(
                    l.custom_card.left + INSET,
                    l.custom_card.bottom - 30.0,
                    l.custom_card.right - INSET,
                    l.custom_card.bottom - 8.0,
                ),
                11,
                false,
                p.sub,
            );
        } else {
            for (i, row) in l.custom_rows.iter().enumerate() {
                let c = &self.settings.custom[i];
                if i > 0 {
                    self.gfx.line(
                        row.left + INSET,
                        row.top,
                        row.right - INSET,
                        row.top,
                        p.border,
                        0.6,
                    );
                }
                self.gfx.text(
                    &c.name.clone(),
                    gfx::rect(
                        row.left + INSET,
                        row.top,
                        row.left + INSET + 130.0,
                        row.bottom,
                    ),
                    // Same 13 px bold as the built-in source rows above.
                    13,
                    true,
                    p.text,
                );
                self.gfx.text(
                    &c.url.clone(),
                    gfx::rect(
                        row.left + INSET + 140.0,
                        row.top,
                        l.custom_remove[i].left - 12.0,
                        row.bottom,
                    ),
                    11,
                    false,
                    p.sub,
                );
                draw_button(
                    &mut self.gfx,
                    p,
                    l.custom_remove[i],
                    "移除",
                    false,
                    self.hover == Some((HIT_SET_REMOVE, i)),
                    false,
                );
            }
        }

        // --- download folder, concurrency, theme ------------------------------
        self.gfx.text(
            "下载与外观",
            gfx::rect(x0, l.dir_header_y, l.x1, l.dir_header_y + 26.0),
            12,
            true,
            p.sub,
        );
        self.gfx.round(l.dir_card, 14.0, p.card);
        self.gfx.outline(l.dir_card, p.border, 0.8);
        let dir_row = gfx::rect(
            x0,
            l.dir_card.top + CARD_PAD,
            l.x1,
            l.dir_card.top + CARD_PAD + ROW,
        );
        self.gfx.text(
            "下载目录",
            gfx::rect(
                dir_row.left + INSET,
                dir_row.top,
                dir_row.left + INSET + LABEL_W,
                dir_row.bottom,
            ),
            12,
            true,
            p.text,
        );
        self.gfx.text(
            &self.settings.download_dir(),
            gfx::rect(
                dir_row.left + INSET + LABEL_W + COL_GAP,
                dir_row.top,
                l.dir_btn.left - 12.0,
                dir_row.bottom,
            ),
            11,
            false,
            p.sub,
        );
        draw_button(
            &mut self.gfx,
            p,
            l.dir_btn,
            "更改…",
            false,
            self.hover == Some((HIT_SET_DIR, 0)),
            false,
        );
        draw_button(
            &mut self.gfx,
            p,
            l.dir_reset,
            "恢复默认",
            false,
            self.hover == Some((HIT_SET_DIR_RESET, 0)),
            self.settings.download_dir.trim().is_empty(),
        );

        let conc_row = gfx::rect(x0, dir_row.bottom, l.x1, dir_row.bottom + ROW);
        self.gfx.line(
            conc_row.left,
            conc_row.top,
            conc_row.right,
            conc_row.top,
            p.border,
            0.6,
        );
        self.gfx.text(
            "同时下载",
            gfx::rect(
                conc_row.left + INSET,
                conc_row.top,
                conc_row.left + INSET + LABEL_W,
                conc_row.bottom,
            ),
            12,
            true,
            p.text,
        );
        let cur = self.settings.concurrency();
        for (i, r) in l.conc_chips.iter().enumerate() {
            let Some(v) = CONCURRENCY.get(i).copied() else {
                break;
            };
            let on = cur == v;
            let hot = self.hover == Some((HIT_SET_CONCURRENCY, v));
            self.gfx.round(
                *r,
                9.0,
                if on {
                    p.accent
                } else if hot {
                    p.hover
                } else {
                    p.subtle
                },
            );
            self.gfx.center_text(
                &v.to_string(),
                *r,
                11,
                on,
                if on { p.on_accent } else { p.text },
            );
        }

        let theme_row = gfx::rect(x0, conc_row.bottom, l.x1, conc_row.bottom + ROW);
        self.gfx.line(
            theme_row.left,
            theme_row.top,
            theme_row.right,
            theme_row.top,
            p.border,
            0.6,
        );
        self.gfx.text(
            "主题",
            gfx::rect(
                theme_row.left + INSET,
                theme_row.top,
                theme_row.left + INSET + LABEL_W,
                theme_row.bottom,
            ),
            12,
            true,
            p.text,
        );
        // Exactly three chips, paired with their labels by the iterator so the
        // slot count and the label array can never drift apart again.
        for (i, (r, label)) in l.theme_chips.iter().zip(THEME_LABELS).enumerate() {
            let on = self.settings.theme
                == match i {
                    1 => Theme::Light,
                    2 => Theme::Dark,
                    _ => Theme::System,
                };
            let hot = self.hover == Some((HIT_SET_THEME, i));
            self.gfx.round(
                *r,
                9.0,
                if on {
                    p.accent
                } else if hot {
                    p.hover
                } else {
                    p.subtle
                },
            );
            self.gfx
                .center_text(label, *r, 11, on, if on { p.on_accent } else { p.text });
        }

        // --- about / self-update ---------------------------------------------
        self.gfx.text(
            "关于",
            gfx::rect(x0, l.about_header_y, l.x1, l.about_header_y + 26.0),
            12,
            true,
            p.sub,
        );
        self.gfx.round(l.about_card, 14.0, p.card);
        self.gfx.outline(l.about_card, p.border, 0.8);
        self.gfx.text(
            &format!("AllStore v{}", crate::selfupdate::current()),
            gfx::rect(
                l.about_card.left + INSET,
                l.about_card.top + 14.0,
                l.about_check.left - 16.0,
                l.about_card.top + 40.0,
            ),
            13,
            true,
            p.text,
        );
        let note = match self.self_update.as_ref() {
            // The release's first paragraph is more useful than a generic
            // "new version available" line.
            Some(rel) if !rel.summary.is_empty() => {
                format!("{}  ·  {}", self.self_update_note, rel.summary)
            }
            _ if self.self_update_note.is_empty() => {
                "启动时自动检查一次，仅读取发布信息".to_string()
            }
            _ => self.self_update_note.clone(),
        };
        self.gfx.text(
            &note,
            gfx::rect(
                l.about_card.left + INSET,
                l.about_card.top + 44.0,
                // Stop at the buttons instead of running underneath them.
                l.about_check.left - 16.0,
                l.about_card.top + 70.0,
            ),
            11,
            false,
            if self.self_update.is_some() {
                p.accent
            } else {
                p.sub
            },
        );
        let has_update = self.self_update.is_some();
        draw_button(
            &mut self.gfx,
            p,
            l.about_check,
            if self.checking_update {
                "检查中…"
            } else {
                "检查更新"
            },
            false,
            self.hover == Some((HIT_SET_SELF_CHECK, 0)),
            self.checking_update,
        );
        let get_label = if self.self_update.as_ref().is_some_and(|r| r.asset.is_some()) {
            "下载安装包"
        } else {
            "前往下载页"
        };
        draw_button(
            &mut self.gfx,
            p,
            l.about_get,
            get_label,
            has_update,
            self.hover == Some((HIT_SET_SELF_GET, 0)),
            !has_update,
        );

        // Section gaps are generous; keep the last card clear of the status bar.
        let _ = (GAP, l.x1 - l.x0);
    }

    /// A rounded single-line input. The caret is drawn at the real text end so
    /// the field looks focused, matching the search box. Same 13 px type as the
    /// search box: it is the same editing primitive, so it must not shrink.
    fn draw_field(&mut self, r: D2D_RECT_F, hint: &str, text: &[u16], focused: bool) {
        let p = self.gfx.pal;
        // A card-coloured fill with a hairline border, like a WinUI TextBox.
        // The old `subtle` fill put the placeholder at only ~3.7:1; on `card`
        // it measures ~4.6:1 (light) and ~5.5:1 (dark).
        self.gfx.round(r, 9.0, p.card);
        self.gfx.outline(
            r,
            if focused { p.accent } else { p.border },
            if focused { 1.2 } else { 0.8 },
        );
        let s = String::from_utf16_lossy(text);
        if s.is_empty() && !focused {
            self.gfx.text(
                hint,
                gfx::rect(r.left + 12.0, r.top, r.right - 12.0, r.bottom),
                13,
                false,
                p.sub,
            );
            return;
        }
        self.gfx.text(
            &s,
            gfx::rect(r.left + 12.0, r.top, r.right - 20.0, r.bottom),
            13,
            false,
            p.text,
        );
        if focused && self.caret_on {
            let w = self.gfx.measure_width(&s, 13, false);
            let x = (r.left + 12.0 + w).min(r.right - 14.0);
            self.gfx
                .line(x, r.top + 9.0, x, r.bottom - 9.0, p.accent, 1.2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A typical maximised-ish window: wide enough that nothing is squeezed.
    const W: f32 = 1240.0;

    #[test]
    fn every_card_shares_one_horizontal_inset() {
        let l = layout_at(W, 0.0, 2);
        assert_eq!(l.add_name.left - l.src_card.left, INSET);
        assert_eq!(l.src_card.right - l.add_btn.right, INSET);
        assert_eq!(l.src_card.right - l.dir_reset.right, INSET);
        assert_eq!(l.src_card.right - l.custom_remove[0].right, INSET);
        assert_eq!(l.src_card.right - l.about_get.right, INSET);
    }

    #[test]
    fn theme_chips_match_their_labels() {
        let l = layout_at(W, 0.0, 0);
        // Four chips with three labels used to index out of bounds on paint.
        assert_eq!(l.theme_chips.len(), THEME_LABELS.len());
        for pair in l.theme_chips.windows(2) {
            assert!(
                pair[0].right < pair[1].left,
                "theme chips overlap: {:?} {:?}",
                pair[0],
                pair[1]
            );
        }
        for r in l.theme_chips.iter().chain(l.conc_chips.iter()) {
            assert!(r.right <= l.dir_card.right - INSET);
        }
    }

    #[test]
    fn empty_custom_card_keeps_the_hint_below_the_fields() {
        let l = layout_at(W, 0.0, 0);
        let hint_top = l.custom_card.bottom - 30.0;
        assert!(
            hint_top > l.add_url.bottom,
            "hint top {hint_top} overlaps fields ending at {}",
            l.add_url.bottom
        );
        assert!(l.custom_card.bottom - 8.0 > hint_top + 20.0);
    }

    #[test]
    fn custom_rows_sit_between_the_fields_and_the_card_bottom() {
        let l = layout_at(W, 0.0, 3);
        assert!(l.custom_rows[0].top > l.add_url.bottom);
        assert!(l.custom_rows.last().unwrap().bottom < l.custom_card.bottom);
        assert_eq!(l.custom_rows.len(), l.custom_remove.len());
        for pair in l.custom_rows.windows(2) {
            assert_eq!(pair[0].bottom, pair[1].top);
        }
    }

    #[test]
    fn about_text_stops_before_the_buttons() {
        let l = layout_at(W, 0.0, 0);
        // The painter clips the title and the note at `about_check.left - 16`;
        // the two buttons must stay to the right of that line.
        assert!(l.about_check.left > l.about_card.left + 240.0);
        assert!(l.about_check.right < l.about_get.left);
        assert!(l.about_get.right <= l.about_card.right - INSET + 0.01);
    }

    #[test]
    fn section_headers_stay_above_their_cards() {
        let l = layout_at(W, 0.0, 1);
        assert!(l.src_header_y + HEADER_H <= l.src_card.top + 0.01);
        assert!(l.custom_header_y + HEADER_H <= l.custom_card.top + 0.01);
        assert!(l.dir_header_y + HEADER_H <= l.dir_card.top + 0.01);
        assert!(l.about_header_y + HEADER_H <= l.about_card.top + 0.01);
    }

    #[test]
    fn chip_rows_start_in_the_directory_path_column() {
        let l = layout_at(W, 0.0, 0);
        // The painter puts the download path at `dir_row.left + INSET + LABEL_W
        // + COL_GAP`; the chip rows must share that column, not sit 20 px left.
        let column = l.dir_card.left + INSET + LABEL_W + COL_GAP;
        assert_eq!(l.conc_chips[0].left, column);
        assert_eq!(l.theme_chips[0].left, column);
    }

    #[test]
    fn scrolling_moves_everything_by_the_same_amount() {
        let a = layout_at(W, 0.0, 1);
        let b = layout_at(W, 120.0, 1);
        for (top, other) in [
            (a.add_btn.top, b.add_btn.top),
            (a.dir_card.top, b.dir_card.top),
            (a.about_card.top, b.about_card.top),
            (a.bottom, b.bottom),
        ] {
            assert!((top - other - 120.0).abs() < 0.01);
        }
    }
}
