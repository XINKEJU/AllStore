//! "我的软件" — the local library.
//!
//! Everything installed on the machine, not just what the store happens to
//! sell: the list comes from the same Uninstall-registry walk that feeds the
//! update matcher. Each row can be launched, revealed in Explorer, updated
//! through the normal download path, or removed with the vendor's own
//! uninstaller after an explicit confirmation.

use std::path::PathBuf;

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONWARNING, MB_YESNO, MessageBoxW, SW_SHOWNORMAL,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::app::{
    AppState, BTN_H, GAP, HIT_LIB_LOCATE, HIT_LIB_OPEN, HIT_LIB_REFRESH, HIT_LIB_UNINSTALL,
    HIT_LIB_UPDATE, Mode, NAV_W, PAD, ROW_H, STATUS_H, TOP_H, draw_button, in_rect,
};
use crate::downloads;
use crate::gfx::{self, Color};
use crate::installed::{self, Installed};

/// One row of the page. Built once per scan so painting does no registry or
/// catalog matching work.
#[derive(Clone)]
pub struct LibRow {
    pub name: String,
    pub version: String,
    pub publisher: String,
    pub launch: Option<PathBuf>,
    pub uninstall: Option<String>,
    /// Set when the store catalog knows a newer build: (download url, version).
    pub update: Option<(String, String)>,
}

impl AppState {
    /// Rebuild the row list from the current inventory and catalog snapshot.
    /// Called on page entry and on "刷新"; the registry walk is the only I/O.
    pub fn rebuild_library(&mut self) {
        let apps = self.inventory.apps();
        let mut rows = Vec::with_capacity(apps.len());
        for a in apps {
            rows.push(lib_row(a, &self.update_catalog));
        }
        self.lib_rows = rows;
    }

    pub fn show_library(&mut self) {
        // Cheap guard: a scan touches every uninstall key (tens of ms), so
        // reuse the snapshot taken at start-up when it is still fresh.
        if self.last_local_scan.elapsed().as_secs() > 60 || self.lib_rows.is_empty() {
            self.refresh_updates_local();
        }
        self.rebuild_library();
        self.scroll = 0;
        self.mode = Mode::Library;
        self.focus_search = false;
        let upd = self.lib_rows.iter().filter(|r| r.update.is_some()).count();
        self.status = format!(
            "我的软件  ·  {} 个已安装  ·  {} 个可更新",
            self.lib_rows.len(),
            upd
        );
        self.invalidate();
    }

    pub(crate) fn lib_row_rect(&self, i: usize) -> Option<D2D_RECT_F> {
        let w = self.width as f32;
        let h = self.height as f32;
        let x0 = NAV_W + PAD;
        let x1 = w - PAD;
        if x1 <= x0 {
            return None;
        }
        let top = TOP_H + 84.0 + i as f32 * (ROW_H + GAP) - self.scroll as f32;
        if top + ROW_H < TOP_H + 70.0 || top > h - STATUS_H {
            return None;
        }
        Some(gfx::rect(x0, top, x1, top + ROW_H))
    }

    /// Four fixed slots, right-aligned: [更新] [打开] [定位] [卸载]. The update
    /// slot is only hit-tested when the row actually has one.
    pub(crate) fn lib_buttons(r: D2D_RECT_F) -> [D2D_RECT_F; 4] {
        // 58 px is enough for the two-character labels at 13 px and leaves the
        // name band wide enough for long product names ("AMD Chipset Software").
        let mut out = [gfx::rect(0.0, 0.0, 0.0, 0.0); 4];
        for (i, slot) in out.iter_mut().enumerate() {
            let x1 = r.right - 16.0 - i as f32 * 66.0;
            *slot = gfx::rect(x1 - 58.0, r.top + 26.0, x1, r.top + 26.0 + BTN_H);
        }
        out
    }

    pub(crate) fn lib_max_scroll(&self) -> i32 {
        let visible = (self.height as f32 - TOP_H - STATUS_H - 110.0).max(0.0);
        let content = self.lib_rows.len() as f32 * (ROW_H + GAP);
        (content - visible).max(0.0) as i32
    }

    pub(crate) fn render_library(&mut self, w: f32, _h: f32) {
        let p = self.gfx.pal;
        let total = self.lib_rows.len();
        let updatable = self.lib_rows.iter().filter(|r| r.update.is_some()).count();
        self.gfx.text(
            "我的软件",
            gfx::rect(NAV_W + PAD, TOP_H + 12.0, w - 260.0, TOP_H + 44.0),
            19,
            true,
            p.text,
        );
        let sub = if total == 0 {
            "尚未扫描到已安装软件".to_string()
        } else if updatable > 0 {
            format!(
                "{} 个已安装  ·  {} 个可更新  ·  含商店目录之外的软件",
                total, updatable
            )
        } else {
            format!(
                "{} 个已安装  ·  全部为最新  ·  仅本地读取，不上传清单",
                total
            )
        };
        self.gfx.text(
            &sub,
            gfx::rect(
                NAV_W + PAD,
                TOP_H + 44.0,
                (w - 260.0).max(NAV_W + PAD + 120.0),
                TOP_H + 68.0,
            ),
            11,
            false,
            p.sub,
        );
        let refresh = gfx::rect(w - 156.0, TOP_H + 16.0, w - PAD, TOP_H + 16.0 + BTN_H);
        draw_button(
            &mut self.gfx,
            p,
            refresh,
            "刷新列表",
            false,
            self.hover == Some((HIT_LIB_REFRESH, 0)),
            false,
        );

        if total == 0 {
            let card = gfx::rect(NAV_W + PAD, TOP_H + 108.0, w - PAD, TOP_H + 220.0);
            self.gfx.shadow(card, 16.0, 0.08);
            self.gfx.vgradient(
                card,
                16.0,
                p.accent_soft.mix(p.accent, 0.12),
                p.accent_soft.mix(p.card, 0.20),
            );
            self.gfx.outline(card, p.accent.with_alpha(0.18), 0.9);
            self.gfx.text(
                "还没有可显示的软件",
                gfx::rect(
                    card.left + 28.0,
                    card.top + 24.0,
                    card.right - 28.0,
                    card.top + 58.0,
                ),
                17,
                true,
                p.text,
            );
            self.gfx.text(
                "点「刷新列表」重新读取本机卸载项",
                gfx::rect(
                    card.left + 28.0,
                    card.top + 62.0,
                    card.right - 28.0,
                    card.top + 92.0,
                ),
                12,
                false,
                p.sub,
            );
            return;
        }

        for i in 0..self.lib_rows.len() {
            let Some(r) = self.lib_row_rect(i) else {
                continue;
            };
            let row = self.lib_rows[i].clone();
            let hovered = self.hover.is_some_and(|(k, n)| n == i && is_lib_hit(k));
            self.gfx
                .round(r, 12.0, if hovered { p.hover } else { p.card });
            self.gfx
                .outline(r, p.border, if hovered { 1.2 } else { 0.8 });

            let icon = gfx::rect(r.left + 16.0, r.top + 16.0, r.left + 68.0, r.top + 68.0);
            self.gfx.vgradient(
                icon,
                12.0,
                p.accent_soft.mix(Color(1.0, 1.0, 1.0, 1.0), 0.10),
                p.accent_soft,
            );
            let ch: String = row.name.chars().take(1).collect();
            self.gfx.center_text(&ch, icon, 22, true, p.accent);

            let slots = Self::lib_buttons(r);
            // Text stops before the left-most button; the "更新" slot is on the
            // far right, so it never pushes the name band around.
            let text_right = slots[3].left - 12.0;
            self.gfx.text(
                &row.name,
                gfx::rect(r.left + 84.0, r.top + 16.0, text_right, r.top + 42.0),
                15,
                true,
                p.text,
            );
            let mut meta = Vec::new();
            if !row.version.trim().is_empty() {
                meta.push(format!("版本 {}", row.version));
            }
            if !row.publisher.trim().is_empty() {
                meta.push(row.publisher.clone());
            }
            if let Some((_, latest)) = &row.update {
                meta.push(format!("可更新到 {}", latest));
            }
            self.gfx.text(
                &meta.join("  ·  "),
                gfx::rect(r.left + 84.0, r.top + 44.0, text_right, r.top + 68.0),
                11,
                false,
                if row.update.is_some() {
                    p.accent
                } else {
                    p.sub
                },
            );

            if row.update.is_some() {
                draw_button(
                    &mut self.gfx,
                    p,
                    slots[0],
                    "更新",
                    true,
                    self.hover == Some((HIT_LIB_UPDATE, i)),
                    false,
                );
            }
            let can_open = row.launch.is_some();
            draw_button(
                &mut self.gfx,
                p,
                slots[1],
                "打开",
                false,
                self.hover == Some((HIT_LIB_OPEN, i)),
                !can_open,
            );
            draw_button(
                &mut self.gfx,
                p,
                slots[2],
                "定位",
                false,
                self.hover == Some((HIT_LIB_LOCATE, i)),
                !can_open,
            );
            draw_button(
                &mut self.gfx,
                p,
                slots[3],
                "卸载",
                false,
                self.hover == Some((HIT_LIB_UNINSTALL, i)),
                row.uninstall.is_none(),
            );
        }
    }
}

fn is_lib_hit(k: u8) -> bool {
    matches!(
        k,
        HIT_LIB_OPEN | HIT_LIB_LOCATE | HIT_LIB_UNINSTALL | HIT_LIB_UPDATE
    )
}

pub(crate) fn hit_targets(st: &AppState, x: f32, y: f32) -> Option<(u8, usize)> {
    let w = st.width as f32;
    if in_rect(
        x,
        y,
        gfx::rect(w - 156.0, TOP_H + 16.0, w - PAD, TOP_H + 16.0 + BTN_H),
    ) {
        return Some((HIT_LIB_REFRESH, 0));
    }
    for i in 0..st.lib_rows.len() {
        let Some(r) = st.lib_row_rect(i) else {
            continue;
        };
        let slots = AppState::lib_buttons(r);
        if st.lib_rows[i].update.is_some() && in_rect(x, y, slots[0]) {
            return Some((HIT_LIB_UPDATE, i));
        }
        if in_rect(x, y, slots[1]) {
            return Some((HIT_LIB_OPEN, i));
        }
        if in_rect(x, y, slots[2]) {
            return Some((HIT_LIB_LOCATE, i));
        }
        if in_rect(x, y, slots[3]) {
            return Some((HIT_LIB_UNINSTALL, i));
        }
    }
    None
}

pub(crate) fn apply(st: &mut AppState, kind: u8, i: usize) {
    if kind == HIT_LIB_REFRESH {
        st.refresh_updates_local();
        st.rebuild_library();
        st.status = format!("已刷新  ·  {} 个已安装软件", st.lib_rows.len());
        st.invalidate();
        return;
    }
    let Some(row) = st.lib_rows.get(i).cloned() else {
        return;
    };
    match kind {
        HIT_LIB_OPEN => {
            let Some(path) = row.launch else {
                return;
            };
            let mut cmd = std::process::Command::new(&path);
            if let Some(dir) = path.parent() {
                cmd.current_dir(dir);
            }
            st.status = match cmd.spawn() {
                Ok(_) => format!("已启动 {}", row.name),
                Err(e) => format!("无法启动 {}：{}", row.name, e),
            };
        }
        HIT_LIB_LOCATE => {
            let Some(path) = row.launch else {
                return;
            };
            downloads::open_folder(&path.to_string_lossy());
            st.status = format!("已在资源管理器中定位 {}", row.name);
        }
        HIT_LIB_UNINSTALL => {
            let Some(cmd) = row.uninstall.clone() else {
                return;
            };
            // Removing software is destructive and irreversible: ask first,
            // in a real modal, and only then hand over to the vendor's own
            // uninstaller (never to the shop's own code).
            let text = format!(
                "确定要卸载「{}」吗？\n\n将运行软件自带的卸载程序，此操作不可撤销。",
                row.name
            );
            let yes = unsafe {
                MessageBoxW(
                    Some(st.hwnd),
                    &HSTRING::from(text),
                    w!("卸载确认"),
                    MB_YESNO | MB_ICONWARNING,
                ) == IDYES
            };
            if !yes {
                st.status = format!("已取消卸载 {}", row.name);
                st.invalidate();
                return;
            }
            match run_uninstaller(&cmd) {
                Ok(()) => {
                    st.status = format!("已启动 {} 的卸载程序，完成后可点「刷新列表」", row.name);
                }
                Err(e) => {
                    st.status = format!("无法启动卸载程序：{}", e);
                }
            }
        }
        HIT_LIB_UPDATE => {
            let Some((url, latest)) = row.update.clone() else {
                return;
            };
            let id = st.new_download(&row.name, &url, None);
            st.start_task(id);
            st.status = format!("开始下载 {} {} 的安装包…", row.name, latest);
        }
        _ => {}
    }
    st.invalidate();
}

/// Vendors ship uninstall strings in every shape: quoted exe with arguments,
/// bare exe, or a `MsiExec.exe` command line. Handing the raw string to
/// `cmd /C` preserves all of them; the executable itself is never rewritten.
fn run_uninstaller(cmd: &str) -> Result<(), String> {
    let line = HSTRING::from(format!("/C {}", cmd));
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("cmd.exe"),
            &line,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value > 32 on success.
    if r.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecute 返回 {}", r.0 as isize))
    }
}

/// Best matching catalog entry for a local app, if it is strictly newer.
fn best_update(catalog: &[crate::model::App], local: &Installed) -> Option<(String, String)> {
    let mut best: Option<&crate::model::App> = None;
    for a in catalog {
        let Some(v) = a.version.as_deref() else {
            continue;
        };
        if v.trim().is_empty() || !installed::name_matches(&a.name, &local.name) {
            continue;
        }
        let better = best
            .and_then(|b| b.version.as_deref())
            .is_none_or(|old| installed::is_newer(v, old));
        if better {
            best = Some(a);
        }
    }
    let a = best?;
    let latest = a.version.clone().unwrap_or_default();
    if !installed::is_newer(&latest, &local.version) {
        return None;
    }
    Some((a.download_url(), latest))
}

fn lib_row(a: &Installed, catalog: &[crate::model::App]) -> LibRow {
    LibRow {
        name: a.name.clone(),
        version: a.version.clone(),
        publisher: a.publisher.clone().unwrap_or_default(),
        launch: installed::launch_path(a),
        uninstall: a.uninstall.clone(),
        update: best_update(catalog, a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::App;

    fn app(name: &str, ver: &str) -> App {
        App {
            id: "L1".into(),
            name: name.into(),
            version: Some(ver.into()),
            dl_a: Some("https://cdn/x.exe".into()),
            ..Default::default()
        }
    }

    #[test]
    fn only_strictly_newer_versions_offer_an_update() {
        let catalog = vec![app("微信 Windows 版", "3.9.10"), app("QQ", "9.9.37")];
        let newer = Installed {
            name: "微信".into(),
            version: "3.9.8".into(),
            ..Default::default()
        };
        let same = Installed {
            name: "QQ".into(),
            version: "9.9.37".into(),
            ..Default::default()
        };
        let unknown = Installed {
            name: "未知软件".into(),
            version: "1.0".into(),
            ..Default::default()
        };
        assert_eq!(
            best_update(&catalog, &newer),
            Some(("https://cdn/x.exe".to_string(), "3.9.10".to_string()))
        );
        assert!(best_update(&catalog, &same).is_none());
        assert!(best_update(&catalog, &unknown).is_none());
    }

    #[test]
    fn row_buttons_stay_inside_the_row() {
        let r = gfx::rect(216.0, 200.0, 1000.0, 284.0);
        let slots = AppState::lib_buttons(r);
        for (i, s) in slots.iter().enumerate() {
            assert!(s.right <= r.right - 16.0 + 0.01, "slot {i} overflows right");
            assert!(s.left > r.left, "slot {i} overflows left");
            assert!(s.top > r.top && s.bottom < r.bottom, "slot {i} vertical");
            if i > 0 {
                assert!(
                    s.right < slots[i - 1].left,
                    "slot {i} overlaps slot {}",
                    i - 1
                );
            }
        }
    }
}
