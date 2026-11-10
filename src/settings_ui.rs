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
const CONCURRENCY: [usize; 4] = [1, 2, 3, 5];

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
    let w = st.width as f32;
    let x0 = NAV_W + PAD;
    let x1 = (w - PAD).max(x0 + 340.0);
    let scroll = st.scroll as f32;
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
    let add_name = gfx::rect(
        x0 + 16.0,
        y + CARD_PAD + 8.0,
        (x0 + 16.0 + 180.0).min(x1 - 420.0),
        y + CARD_PAD + 8.0 + 36.0,
    );
    let add_url = gfx::rect(
        add_name.right + 10.0,
        add_name.top,
        x1 - 16.0 - 96.0,
        add_name.bottom,
    );
    let add_btn = gfx::rect(x1 - 16.0 - 88.0, add_name.top, x1 - 16.0, add_name.bottom);
    let custom_n = st.settings.custom.len() as f32;
    let custom_card = gfx::rect(
        x0,
        y,
        x1,
        y + CARD_PAD
            + 52.0
            + if custom_n > 0.0 {
                8.0 + custom_n * 46.0
            } else {
                0.0
            }
            + CARD_PAD,
    );
    let first = custom_card.top + CARD_PAD + 52.0 + 8.0;
    let custom_rows: Vec<D2D_RECT_F> = (0..st.settings.custom.len())
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
                r.right - 16.0 - 76.0,
                r.top + 9.0,
                r.right - 16.0,
                r.top + 37.0,
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
        x1 - 16.0 - 96.0,
        dir_row.top + 12.0,
        x1 - 16.0,
        dir_row.top + 12.0 + BTN_H,
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
    let chips = |base: &D2D_RECT_F| -> Vec<D2D_RECT_F> {
        (0..4)
            .map(|i| {
                chip(
                    base.left + 170.0 + i as f32 * 76.0,
                    chip_y.max(base.top + 12.0),
                    68.0,
                )
            })
            .collect()
    };
    let conc_chips = chips(&conc_row);
    let theme_chips = chips(&theme_row);

    let about_header_y = dir_card.bottom + SECTION_GAP;
    y = about_header_y + HEADER_H;
    let about_card = gfx::rect(x0, y, x1, y + 96.0);
    let about_get = gfx::rect(
        about_card.right - 16.0 - 104.0,
        about_card.top + 32.0,
        about_card.right - 16.0,
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
                self.gfx
                    .line(row.left, row.top, row.right, row.top, p.border, 0.6);
            }
            self.gfx.text(
                BUILTIN[i].0,
                gfx::rect(row.left + 20.0, row.top, row.right - 160.0, row.bottom),
                13,
                true,
                p.text,
            );
            let pill = gfx::rect(
                row.right - 92.0,
                row.top + (ROW - 28.0) * 0.5,
                row.right - 20.0,
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
                false,
                if on { p.on_accent } else { p.sub },
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
                    l.custom_card.left + 16.0,
                    l.custom_card.bottom - 26.0,
                    l.custom_card.right - 16.0,
                    l.custom_card.bottom - 6.0,
                ),
                10,
                false,
                p.sub,
            );
        } else {
            for (i, row) in l.custom_rows.iter().enumerate() {
                let c = &self.settings.custom[i];
                if i > 0 {
                    self.gfx.line(
                        row.left + 16.0,
                        row.top,
                        row.right - 16.0,
                        row.top,
                        p.border,
                        0.6,
                    );
                }
                self.gfx.text(
                    &c.name.clone(),
                    gfx::rect(row.left + 16.0, row.top, row.left + 150.0, row.bottom),
                    12,
                    true,
                    p.text,
                );
                self.gfx.text(
                    &c.url.clone(),
                    gfx::rect(row.left + 160.0, row.top, row.right - 110.0, row.bottom),
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
                dir_row.left + 20.0,
                dir_row.top,
                dir_row.left + 150.0,
                dir_row.bottom,
            ),
            12,
            true,
            p.text,
        );
        self.gfx.text(
            &self.settings.download_dir(),
            gfx::rect(
                dir_row.left + 170.0,
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
                conc_row.left + 20.0,
                conc_row.top,
                conc_row.left + 150.0,
                conc_row.bottom,
            ),
            12,
            true,
            p.text,
        );
        let cur = self.settings.concurrency();
        for (i, r) in l.conc_chips.iter().enumerate() {
            let v = CONCURRENCY[i];
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
                theme_row.left + 20.0,
                theme_row.top,
                theme_row.left + 150.0,
                theme_row.bottom,
            ),
            12,
            true,
            p.text,
        );
        for (i, r) in l.theme_chips.iter().enumerate() {
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
            self.gfx.center_text(
                ["跟随系统", "浅色", "深色"][i],
                *r,
                11,
                on,
                if on { p.on_accent } else { p.text },
            );
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
            &format!("AllStore  v{}", crate::selfupdate::current()),
            gfx::rect(
                l.about_card.left + 20.0,
                l.about_card.top + 14.0,
                l.about_card.right - 220.0,
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
                l.about_card.left + 20.0,
                l.about_card.top + 44.0,
                l.about_card.right - 20.0,
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
    /// the field looks focused, matching the search box.
    fn draw_field(&mut self, r: D2D_RECT_F, hint: &str, text: &[u16], focused: bool) {
        let p = self.gfx.pal;
        self.gfx.round(r, 9.0, p.subtle);
        if focused {
            self.gfx.outline(r, p.accent, 1.2);
        }
        let s = String::from_utf16_lossy(text);
        if s.is_empty() && !focused {
            self.gfx.text(
                hint,
                gfx::rect(r.left + 12.0, r.top, r.right - 12.0, r.bottom),
                11,
                false,
                p.sub,
            );
            return;
        }
        self.gfx.text(
            &s,
            gfx::rect(r.left + 12.0, r.top, r.right - 20.0, r.bottom),
            11,
            false,
            p.text,
        );
        if focused && self.caret_on {
            let w = self.gfx.measure_width(&s, 11, false);
            let x = (r.left + 12.0 + w).min(r.right - 14.0);
            self.gfx
                .line(x, r.top + 10.0, x, r.bottom - 10.0, p.accent, 1.2);
        }
    }
}
