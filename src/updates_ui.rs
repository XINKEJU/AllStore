//! The "software updates" page: layout, interaction and a hardened downloader
//! shared by normal installs and updates.

use std::cell::Cell;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

use crate::app::{
    AppState, BTN_H, GAP, HIT_ALL, HIT_CHECK, HIT_IGNORED_ALL, HIT_IGNORED_LIST,
    HIT_IGNORED_RESTORE, HIT_ROW, HIT_ROW_IGNORE, HIT_ROW_UPDATE, Mode, NAV_W, PAD, Payload, ROW_H,
    STATUS_H, TOP_H, WM_APP_DL_DONE, WM_APP_DL_PROGRESS, WM_APP_STATUS, WM_APP_UPDATE_DONE,
    WM_APP_UPDATE_PROGRESS, draw_button, in_rect, post,
};
use crate::downloads::{DlState, Progress};
use crate::gfx;
use crate::installed::{self, Installed, Inventory};
use crate::net;
use crate::update::{self, UpdateItem};

fn sanitize(name: &str) -> String {
    let out: String = name
        .chars()
        .map(|c| {
            if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut out = out.trim().trim_end_matches(['.', ' ']).to_string();
    if out.is_empty() {
        return "update".to_string();
    }
    let upper = out.to_ascii_uppercase();
    let stem = upper.split('.').next().unwrap_or("");
    const RESERVED: [&str; 8] = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "LPT1", "LPT2"];
    if RESERVED.contains(&stem) {
        out.insert(0, '_');
    }
    let mut short = String::new();
    for (i, c) in out.chars().enumerate() {
        if i >= 120 {
            break;
        }
        short.push(c);
    }
    short
}

/// Use the extension of the file the CDN actually serves. Writing every
/// package as `.exe` breaks `.msi`/`.zip` installers.
fn extension_of(url: &str) -> String {
    let path = url
        .split('?')
        .next()
        .unwrap_or(url)
        .split('#')
        .next()
        .unwrap_or(url);
    let file_name = path.rsplit('/').next().unwrap_or("");
    let ext = file_name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "exe" | "msi" | "zip" | "7z" | "rar" | "apk" => format!(".{}", ext),
        _ => ".exe".to_string(),
    }
}

fn unique_path(dir: &str, stem: &str, ext: &str) -> String {
    let mut candidate = format!("{}\\{}{}", dir, stem, ext);
    let mut n = 2;
    while std::path::Path::new(&candidate).exists() {
        candidate = format!("{}\\{} ({}){}", dir, stem, n, ext);
        n += 1;
        if n > 999 {
            break;
        }
    }
    candidate
}

/// Incomplete downloads live in the app's own temp folder, not in Downloads:
/// a cancelled or killed transfer used to leave multi-MB `.part` files in the
/// user's Downloads folder forever.
fn temp_dir() -> std::path::PathBuf {
    let p = std::env::temp_dir().join("MindosShop");
    let _ = std::fs::create_dir_all(&p);
    p
}

pub(crate) fn cleanup_temp() {
    let Ok(entries) = std::fs::read_dir(temp_dir()) else {
        return;
    };
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .saturating_sub(6 * 3600);
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        let age = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if age < cutoff {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// rename() fails across volumes; copy is the portable fallback.
fn persist(tmp: &str, path: &str) -> Result<(), String> {
    match std::fs::rename(tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(tmp, path).map_err(|e| format!("无法保存安装包：{}", e))?;
            let _ = std::fs::remove_file(tmp);
            Ok(())
        }
    }
}

/// Reject HTML error pages / gateway interstitials that would otherwise be
/// saved as a plausible-looking `.exe`.
fn looks_like_package(file: &mut File, ext: &str) -> bool {
    let mut head = [0u8; 4];
    if file.read_exact(&mut head).is_err() {
        return false;
    }
    match ext {
        ".exe" => head[0] == b'M' && head[1] == b'Z',
        ".msi" => head == [0xD0, 0xCF, 0x11, 0xE0],
        ".zip" | ".apk" => head[0] == b'P' && head[1] == b'K',
        ".7z" => head[0] == b'7' && head[1] == b'z',
        _ => true,
    }
}

/// Stream a package to Downloads into a temporary `.part` file, then rename
/// it only after the size, status code and file signature all check out.
#[cfg(test)]
pub(crate) fn download_package(name: &str, url: &str, h: usize) -> Result<String, String> {
    download_package_ex(name, url, h, None, None)
}

/// Task-aware variant. `Content-Length` is reported before the first byte and
/// every chunk feeds a throttled progress emitter, so the UI always has a
/// meaningful percentage — the previous code only posted once per 8 MB and
/// therefore showed nothing at all for typical 1-3 MB installers.
pub(crate) fn download_package_ex(
    name: &str,
    url: &str,
    h: usize,
    task_id: Option<u64>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<String, String> {
    let safe = sanitize(name);
    let dir = match std::env::var("USERPROFILE") {
        Ok(p) => format!("{}\\Downloads", p),
        Err(_) => ".".to_string(),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建下载目录：{}", e))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = temp_dir()
        .join(format!(".{}-{}.part", safe, stamp))
        .to_string_lossy()
        .to_string();
    let mut file = File::create(&tmp).map_err(|e| format!("无法创建文件：{}", e))?;
    let mut total = 0usize;
    let mut write_error: Option<String> = None;
    let mut progress = Progress::new(task_id, h, name);
    // The header callback and the body sink both run on this thread; a Cell
    // hands Content-Length from one closure to the other without aliasing.
    let total_hint = Cell::new(0u64);

    let fetch = {
        let mut meta = |size: u64| total_hint.set(size);
        let mut sink = |chunk: &[u8]| -> bool {
            if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                return false;
            }
            if write_error.is_some() {
                return false;
            }
            if let Err(e) = file.write_all(chunk) {
                write_error = Some(e.to_string());
                return false;
            }
            total += chunk.len();
            if progress.total() == 0 && total_hint.get() > 0 {
                progress.set_total(total_hint.get());
            }
            progress.written(chunk.len());
            progress.add(chunk.len());
            true
        };
        net::fetch_binary_stream(url, &mut sink, &mut meta)
    };
    let _ = file.flush();
    drop(file);

    let outcome = finalize(&fetch, total, write_error, &tmp, &dir, &safe, name);
    if outcome.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    outcome
}

fn finalize(
    fetch: &Result<net::Fetch, String>,
    total: usize,
    write_error: Option<String>,
    tmp: &str,
    dir: &str,
    safe: &str,
    name: &str,
) -> Result<String, String> {
    if let Some(e) = write_error {
        return Err(format!("写盘失败：{}", e));
    }
    let meta = fetch.as_ref().map_err(|e| e.clone())?;
    if total == 0 {
        return Err("下载内容为空（服务器未返回安装包）".into());
    }
    if meta.size > 0 && meta.size != total as u64 {
        return Err(format!(
            "下载不完整：应为 {} 字节，实际 {} 字节",
            meta.size, total
        ));
    }
    let ext = extension_of(&meta.final_url);
    let mut probe = File::open(tmp).map_err(|e| format!("无法校验安装包：{}", e))?;
    if !looks_like_package(&mut probe, &ext) {
        return Err("下载内容不是有效的安装包（可能被网络拦截）".into());
    }
    drop(probe);
    let path = unique_path(dir, safe, &ext);
    persist(tmp, &path)?;
    net::log(&format!(
        "DOWNLOAD ok name={} bytes={} ext={} host={}",
        name,
        total,
        ext,
        meta.final_url.split('/').nth(2).unwrap_or("?")
    ));
    Ok(path)
}

impl AppState {
    pub(crate) fn show_updates(&mut self) {
        // Entering the page from the sidebar always shows the actionable list;
        // the ignored list is only one click away from its button.
        self.ignored_view = false;
        self.scroll = 0;
        // Without a cached catalog there is nothing to re-match locally.
        if self.update_catalog.is_empty() || self.update_checked_at == 0 {
            self.start_update_check();
            return;
        }
        // The user may have just run an installer. Re-read the uninstall keys
        // before showing the count; this is a local registry scan, no network.
        self.refresh_updates_local();
        let stale = update::now().saturating_sub(self.update_checked_at) > 6 * 3600;
        if stale && !self.update_checking {
            self.start_update_check();
            return;
        }
        let hours = update::now().saturating_sub(self.update_checked_at) / 3600;
        self.status = format!(
            "软件更新  ·  {} 个可更新  ·  本机 {} 款已装  ·  目录 {} 小时前",
            self.updates.len(),
            self.inventory.apps().len(),
            hours
        );
        self.invalidate();
    }

    /// Fast local-only refresh: no catalog request, no data leaves the machine.
    /// The single registry walk also refreshes the store's "installed" badges.
    /// Returns true when the visible update list actually changed.
    pub(crate) fn refresh_updates_local(&mut self) -> bool {
        self.set_local_snapshot(installed::scan())
    }

    fn set_local_snapshot(&mut self, snap: Vec<Installed>) -> bool {
        self.inventory = Inventory::new(snap);
        self.last_local_scan = Instant::now();
        if self.update_catalog.is_empty() {
            return false;
        }
        let items = update::rescan_with(&self.update_catalog, self.inventory.apps(), &self.ignored);
        let changed = items.len() != self.updates.len()
            || items.iter().zip(self.updates.iter()).any(|(a, b)| {
                a.soft_id != b.soft_id || a.current != b.current || a.latest != b.latest
            });
        self.updates = items;
        if changed {
            self.save_update_cache();
        }
        changed
    }

    pub(crate) fn start_update_check(&mut self) {
        if self.update_checking {
            return;
        }
        if self.token.is_empty() {
            self.status = "仍在初始化，请稍候再检查更新".into();
            self.invalidate();
            return;
        }
        self.update_checking = true;
        self.update_progress = (0, 1);
        self.status = "正在检查软件更新（本地比对，不上传本机软件清单）…".into();
        self.invalidate();
        let h = self.hwnd_usize();
        let token = self.token.clone();
        let ignored = self.ignored.clone();
        std::thread::spawn(move || {
            let mut report = |done: usize, total: usize| {
                post(
                    h,
                    WM_APP_UPDATE_PROGRESS,
                    Payload::UpdateProgress(done, total),
                );
            };
            let result = update::check(&token, &mut report).map(|mut r| {
                r.items.retain(|i| !ignored.contains(&i.soft_id));
                r
            });
            post(h, WM_APP_UPDATE_DONE, Payload::UpdateDone(result));
        });
    }

    pub(crate) fn on_update_progress(&mut self, done: usize, total: usize) {
        self.update_progress = (done, total);
        self.status = format!("正在检查软件更新… {}/{}", done, total);
        self.invalidate();
    }

    pub(crate) fn on_update_done(&mut self, result: Result<update::CheckResult, String>) {
        self.update_checking = false;
        match result {
            Ok(r) => {
                self.update_catalog = r.catalog;
                self.updates = r.items;
                self.inventory = Inventory::new(r.local);
                self.last_local_scan = Instant::now();
                self.update_checked_at = update::now();
                self.save_update_cache();
                self.queue_icons();
                self.status = format!("检查完成  ·  发现 {} 个可更新应用", self.updates.len());
            }
            Err(e) => self.status = format!("更新检查失败：{}", e),
        }
        self.invalidate();
    }

    fn save_update_cache(&self) {
        update::save_cache(&update::Cache {
            checked_at: self.update_checked_at,
            items: self.updates.clone(),
            ignored: self.ignored.clone(),
            ignored_items: self.ignored_items.clone(),
            catalog: self.update_catalog.clone(),
        });
    }

    /// The third header button. Always visible (even at zero ignored apps) so
    /// the place to restore an ignored update is discoverable.
    pub(crate) fn ignored_rect(&self) -> D2D_RECT_F {
        Self::ignored_button_rect(self.width as f32)
    }

    pub(crate) fn ignored_button_rect(w: f32) -> D2D_RECT_F {
        gfx::rect(w - 400.0, TOP_H + 16.0, w - 290.0, TOP_H + 16.0 + BTN_H)
    }

    pub(crate) fn check_rect(&self) -> D2D_RECT_F {
        Self::check_button_rect(self.width as f32)
    }

    pub(crate) fn check_button_rect(w: f32) -> D2D_RECT_F {
        gfx::rect(w - 280.0, TOP_H + 16.0, w - 166.0, TOP_H + 16.0 + BTN_H)
    }

    pub(crate) fn all_rect(&self) -> D2D_RECT_F {
        Self::all_button_rect(self.width as f32)
    }

    pub(crate) fn all_button_rect(w: f32) -> D2D_RECT_F {
        gfx::rect(w - 156.0, TOP_H + 16.0, w - PAD, TOP_H + 16.0 + BTN_H)
    }

    /// Rows for the ignored list. Older caches only stored ids: names are
    /// recovered from the cached catalog, with an id fallback so nothing is
    /// ever invisible or un-restorable.
    pub(crate) fn ignored_entries(&self) -> Vec<UpdateItem> {
        self.ignored
            .iter()
            .map(|id| {
                if let Some(x) = self.ignored_items.iter().find(|x| &x.soft_id == id) {
                    return x.clone();
                }
                if let Some(a) = self.update_catalog.iter().find(|a| &a.id == id) {
                    return UpdateItem {
                        soft_id: a.id.clone(),
                        name: a.name.clone(),
                        current: String::new(),
                        latest: a.version.clone().unwrap_or_default(),
                        size: a.size.clone(),
                        logo: a.logo_url().map(str::to_string),
                        url: a.download_url(),
                    };
                }
                UpdateItem {
                    soft_id: id.clone(),
                    name: format!("应用 {}", id),
                    current: String::new(),
                    latest: String::new(),
                    size: None,
                    logo: None,
                    url: String::new(),
                }
            })
            .collect()
    }

    fn restore_ignored(&mut self, i: usize) {
        let Some(entry) = update::unignore(&mut self.ignored, &mut self.ignored_items, i) else {
            return;
        };
        // Local-only re-scan: if a newer version is still available the row
        // comes back into the update list immediately.
        self.refresh_updates_local();
        self.save_update_cache();
        self.status = format!("已恢复 {} 的更新提醒", entry.name);
        self.invalidate();
    }

    fn restore_all(&mut self) {
        let n = self.ignored.len();
        if n == 0 {
            return;
        }
        self.ignored.clear();
        self.ignored_items.clear();
        self.ignored_view = false;
        self.scroll = 0;
        self.refresh_updates_local();
        self.save_update_cache();
        self.status = format!("已恢复全部 {} 项更新提醒", n);
        self.invalidate();
    }

    fn toggle_ignored_view(&mut self) {
        self.ignored_view = !self.ignored_view;
        self.scroll = 0;
        self.status = if self.ignored_view {
            format!(
                "已忽略 {} 项  ·  点「恢复」重新接收更新提醒",
                self.ignored.len()
            )
        } else {
            format!("软件更新  ·  {} 个可更新", self.updates.len())
        };
        self.invalidate();
    }

    pub(crate) fn row_rect(&self, i: usize) -> Option<D2D_RECT_F> {
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

    pub(crate) fn row_buttons(r: D2D_RECT_F) -> (D2D_RECT_F, D2D_RECT_F) {
        (
            gfx::rect(
                r.right - 94.0,
                r.top + 26.0,
                r.right - 16.0,
                r.top + 26.0 + BTN_H,
            ),
            gfx::rect(
                r.right - 176.0,
                r.top + 26.0,
                r.right - 104.0,
                r.top + 26.0 + BTN_H,
            ),
        )
    }

    pub(crate) fn update_max_scroll(&self) -> i32 {
        let visible = (self.height as f32 - TOP_H - STATUS_H - 100.0).max(0.0);
        let rows = if self.ignored_view {
            self.ignored.len()
        } else {
            self.updates.len()
        };
        let content = rows as f32 * (ROW_H + GAP);
        (content - visible).max(0.0) as i32
    }

    pub(crate) fn render_updates(&mut self, w: f32, _h: f32) {
        let p = self.gfx.pal;
        let viewing_ignored = self.ignored_view;
        let rows: Vec<UpdateItem> = if viewing_ignored {
            self.ignored_entries()
        } else {
            self.updates.clone()
        };
        self.gfx.text(
            if viewing_ignored {
                "已忽略的更新"
            } else {
                "软件更新"
            },
            gfx::rect(NAV_W + PAD, TOP_H + 12.0, w - 420.0, TOP_H + 44.0),
            19,
            true,
            p.text,
        );
        // Three header buttons leave a narrow caption band on an 830 DIP
        // window; use short wording there instead of ellipsis clipping.
        let narrow = w < 1000.0;
        let sub = if viewing_ignored {
            if narrow {
                format!("{} 项已忽略  ·  点「恢复」重新提醒", rows.len())
            } else {
                format!("{} 项已忽略  ·  恢复后将重新出现在更新列表", rows.len())
            }
        } else if self.update_checking {
            if narrow {
                format!(
                    "正在检查… {}/{}",
                    self.update_progress.0, self.update_progress.1
                )
            } else {
                format!(
                    "正在检查… {}/{}（本地比对，不上传本机软件清单）",
                    self.update_progress.0, self.update_progress.1
                )
            }
        } else if self.update_checked_at == 0 {
            if narrow {
                "点击「检查更新」扫描本机".to_string()
            } else {
                "尚未检查  ·  点击「检查更新」扫描本机已安装软件".to_string()
            }
        } else {
            let hours = update::now().saturating_sub(self.update_checked_at) / 3600;
            if narrow {
                format!("{} 个可更新  ·  {} 小时前检查", self.updates.len(), hours)
            } else {
                format!(
                    "{} 个可更新  ·  上次检查 {} 小时前  ·  仅本地比对",
                    self.updates.len(),
                    hours
                )
            }
        };
        self.gfx.text(
            &sub,
            gfx::rect(
                NAV_W + PAD,
                TOP_H + 44.0,
                (w - 420.0).max(NAV_W + PAD + 120.0),
                TOP_H + 68.0,
            ),
            11,
            false,
            p.sub,
        );

        let ignored_label = if viewing_ignored {
            "返回更新".to_string()
        } else {
            format!("已忽略 {}", self.ignored.len())
        };
        let gr = self.ignored_rect();
        draw_button(
            &mut self.gfx,
            p,
            gr,
            &ignored_label,
            false,
            self.hover == Some((HIT_IGNORED_LIST, 0)),
            false,
        );
        let cr = self.check_rect();
        draw_button(
            &mut self.gfx,
            p,
            cr,
            if self.update_checking {
                "检查中…"
            } else {
                "检查更新"
            },
            true,
            self.hover == Some((HIT_CHECK, 0)),
            self.update_checking,
        );
        let ar = self.all_rect();
        draw_button(
            &mut self.gfx,
            p,
            ar,
            if viewing_ignored {
                "全部恢复"
            } else {
                "全部更新"
            },
            false,
            self.hover
                == Some((
                    if viewing_ignored {
                        HIT_IGNORED_ALL
                    } else {
                        HIT_ALL
                    },
                    0,
                )),
            if viewing_ignored {
                self.ignored.is_empty()
            } else {
                self.updates.is_empty() || self.update_checking
            },
        );

        if rows.is_empty() && (!self.update_checking || viewing_ignored) {
            let card = gfx::rect(NAV_W + PAD, TOP_H + 108.0, w - PAD, TOP_H + 224.0);
            self.gfx.round(card, 16.0, p.accent_soft);
            self.gfx.outline(card, p.border, 0.8);
            self.gfx.text(
                if viewing_ignored {
                    "没有被忽略的应用"
                } else {
                    "未发现可更新应用"
                },
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
                if viewing_ignored {
                    "点「忽略」跳过的软件会列在这里，随时可以恢复更新提醒"
                } else {
                    "检查更新会比对商店目录与本机已安装软件，全程在本地完成"
                },
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
            return;
        }

        for (i, u) in rows.iter().enumerate() {
            let Some(r) = self.row_rect(i) else {
                continue;
            };
            let row_hover = self.hover.is_some_and(|(k, n)| {
                n == i
                    && ((viewing_ignored && k == HIT_IGNORED_RESTORE)
                        || (!viewing_ignored
                            && matches!(k, HIT_ROW | HIT_ROW_UPDATE | HIT_ROW_IGNORE)))
            });
            self.gfx
                .round(r, 12.0, if row_hover { p.hover } else { p.card });
            self.gfx
                .outline(r, p.border, if row_hover { 1.2 } else { 0.8 });
            let icon = gfx::rect(r.left + 16.0, r.top + 16.0, r.left + 68.0, r.top + 68.0);
            let key = u.logo.as_deref().unwrap_or("");
            if !self.gfx.bitmap(key, icon) {
                self.gfx.round(icon, 12.0, p.accent_soft);
                let ch: String = u.name.chars().take(1).collect();
                self.gfx.center_text(&ch, icon, 22, true, p.accent);
            }
            let (upd, ign) = Self::row_buttons(r);
            let text_right = if viewing_ignored {
                upd.left - 12.0
            } else {
                ign.left - 12.0
            };
            self.gfx.text(
                &u.name,
                gfx::rect(r.left + 84.0, r.top + 12.0, text_right, r.top + 40.0),
                14,
                true,
                p.text,
            );
            let version_line = if viewing_ignored {
                if u.latest.is_empty() {
                    "已忽略更新提示  ·  点「恢复」重新接收提醒".to_string()
                } else {
                    format!("商店版本 {}  ·  已忽略更新提示", u.latest)
                }
            } else {
                format!("当前 {}   →   新版 {}", u.current, u.latest)
            };
            self.gfx.text(
                &version_line,
                gfx::rect(r.left + 84.0, r.top + 42.0, text_right, r.top + 68.0),
                12,
                false,
                p.sub,
            );
            if viewing_ignored {
                draw_button(
                    &mut self.gfx,
                    p,
                    upd,
                    "恢复",
                    true,
                    self.hover == Some((HIT_IGNORED_RESTORE, i)),
                    false,
                );
            } else {
                // Reflect the shared task state (same task list as the cards
                // and download centre) so a finished update offers "安装".
                let task_state = self.task_for(&u.name).map(|t| t.state);
                let (action, prominent) = match task_state {
                    Some(DlState::Downloading | DlState::Queued) => ("取消", false),
                    Some(DlState::Done) => ("安装", true),
                    Some(DlState::Failed | DlState::Canceled) => ("重试", true),
                    None => ("更新", true),
                };
                draw_button(
                    &mut self.gfx,
                    p,
                    ign,
                    "忽略",
                    false,
                    self.hover == Some((HIT_ROW_IGNORE, i)),
                    false,
                );
                draw_button(
                    &mut self.gfx,
                    p,
                    upd,
                    action,
                    prominent,
                    self.hover == Some((HIT_ROW_UPDATE, i)),
                    false,
                );
            }
        }
    }

    fn start_update_download(&mut self, i: usize) {
        let Some(u) = self.updates.get(i).cloned() else {
            return;
        };
        let id = self.new_download(&u.name, &u.url, u.logo.clone());
        self.start_task(id);
    }

    /// The update row's primary button mirrors the store card: it cancels an
    /// active job, retries a failed one, installs a finished package, or starts
    /// a fresh download when nothing is pending for this app yet.
    fn row_primary_action(&mut self, i: usize) {
        let Some(u) = self.updates.get(i).cloned() else {
            return;
        };
        let existing = self.task_for(&u.name).map(|t| (t.id, t.state));
        match existing {
            Some((id, DlState::Downloading | DlState::Queued)) => self.cancel_task(id),
            Some((id, DlState::Failed | DlState::Canceled)) => self.retry_task(id),
            Some((id, DlState::Done)) => self.install_task(id),
            None => self.start_update_download(i),
        }
    }

    fn update_all(&mut self) {
        let items: Vec<(String, String, Option<String>)> = self
            .updates
            .iter()
            .map(|u| (u.name.clone(), u.url.clone(), u.logo.clone()))
            .collect();
        if items.is_empty() {
            return;
        }
        let h = self.hwnd_usize();
        let mut jobs: Vec<(u64, String, String, Arc<AtomicBool>)> = Vec::new();
        for (name, url, logo) in items {
            let id = self.new_download(&name, &url, logo);
            if jobs.iter().any(|j| j.0 == id) {
                continue;
            }
            let cancel = self
                .downloads
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.cancel.clone())
                .unwrap_or_default();
            jobs.push((id, name, url, cancel));
        }
        let ids: Vec<u64> = jobs.iter().map(|j| j.0).collect();
        self.mark_running(&ids);
        let n = jobs.len();
        self.status = format!("已加入下载队列：{} 个更新包（可在下载管理查看进度）", n);
        self.invalidate();
        // Still serial: one connection at a time, but each package now has a
        // full task entry with its own live progress in the download centre.
        std::thread::spawn(move || {
            let mut ok = 0usize;
            let mut failures: Vec<String> = Vec::new();
            for (i, (id, name, url, cancel)) in jobs.into_iter().enumerate() {
                post(
                    h,
                    WM_APP_STATUS,
                    Payload::Status(format!("({}/{}) 正在下载 {} …", i + 1, n, name)),
                );
                post(
                    h,
                    WM_APP_DL_PROGRESS,
                    Payload::DlProgress {
                        id,
                        done: 0,
                        total: 0,
                        speed: 0.0,
                        disk: 0.0,
                    },
                );
                let result = download_package_ex(&name, &url, h, Some(id), Some(cancel));
                match &result {
                    Ok(_) => ok += 1,
                    Err(e) => {
                        net::log(&format!("UPDATE FAIL {} {}", name, e));
                        failures.push(format!("{}：{}", name, e));
                    }
                }
                post(h, WM_APP_DL_DONE, Payload::DlDone { id, result });
            }
            let msg = if failures.is_empty() {
                format!(
                    "已完成 {}/{} 个更新包，请在下载管理或 Downloads 中运行安装程序",
                    ok, n
                )
            } else {
                format!(
                    "已完成 {}/{}，失败 {} 个 — {}",
                    ok,
                    n,
                    failures.len(),
                    failures.join("；").chars().take(160).collect::<String>()
                )
            };
            post(h, WM_APP_STATUS, Payload::Status(msg));
        });
    }

    fn ignore_update(&mut self, i: usize) {
        if i >= self.updates.len() {
            return;
        }
        let u = self.updates.remove(i);
        if !self.ignored.contains(&u.soft_id) {
            self.ignored.push(u.soft_id.clone());
        }
        self.ignored_items.retain(|x| x.soft_id != u.soft_id);
        self.ignored_items.push(u.clone());
        self.save_update_cache();
        self.status = format!(
            "已忽略 {} 的更新提示  ·  可在「已忽略 {}」中恢复",
            u.name,
            self.ignored.len()
        );
        self.invalidate();
    }
}

/// Hit targets for the updates page, consumed by the shared hit-test in app.rs.
pub(crate) fn hit_targets(st: &AppState, x: f32, y: f32) -> Option<(u8, usize)> {
    if in_rect(x, y, st.ignored_rect()) {
        return Some((HIT_IGNORED_LIST, 0));
    }
    if in_rect(x, y, st.check_rect()) {
        return Some((HIT_CHECK, 0));
    }
    if in_rect(x, y, st.all_rect()) {
        return Some((
            if st.ignored_view {
                HIT_IGNORED_ALL
            } else {
                HIT_ALL
            },
            0,
        ));
    }
    let rows = if st.ignored_view {
        st.ignored.len()
    } else {
        st.updates.len()
    };
    for i in 0..rows {
        if let Some(r) = st.row_rect(i) {
            let (upd, ign) = AppState::row_buttons(r);
            if st.ignored_view {
                if in_rect(x, y, upd) || in_rect(x, y, r) {
                    return Some((HIT_IGNORED_RESTORE, i));
                }
                continue;
            }
            if in_rect(x, y, upd) {
                return Some((HIT_ROW_UPDATE, i));
            }
            if in_rect(x, y, ign) {
                return Some((HIT_ROW_IGNORE, i));
            }
            if in_rect(x, y, r) {
                return Some((HIT_ROW, i));
            }
        }
    }
    None
}

pub(crate) fn apply(st: &mut AppState, kind: u8, i: usize) {
    match kind {
        HIT_CHECK => st.start_update_check(),
        HIT_ALL => st.update_all(),
        HIT_IGNORED_LIST => st.toggle_ignored_view(),
        HIT_IGNORED_ALL => st.restore_all(),
        HIT_IGNORED_RESTORE => st.restore_ignored(i),
        HIT_ROW_UPDATE => st.row_primary_action(i),
        HIT_ROW_IGNORE => st.ignore_update(i),
        HIT_ROW => {
            if let Some(u) = st.updates.get(i) {
                let kw = u.name.clone();
                st.search = kw.encode_utf16().collect();
                st.mode = Mode::Browse;
                st.active_nav = 0;
                st.scroll = 0;
                st.do_search();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_comes_from_the_final_url() {
        assert_eq!(extension_of("http://cdn/x/Setup%20v1.2.exe"), ".exe");
        assert_eq!(extension_of("http://cdn/x/app.msi?token=abc"), ".msi");
        assert_eq!(extension_of("http://cdn/x/a.zip"), ".zip");
        assert_eq!(extension_of("http://cdn/x/download?softid=1"), ".exe");
    }

    /// Regression for "the progress bar has no denominator": Content-Length
    /// must be known before the first body chunk, even across the 302 hop.
    #[test]
    #[ignore = "requires live network"]
    fn live_headers_report_size_before_the_body() {
        use std::cell::Cell;
        let url = format!("{}/dlservice/download?softid=L118939", crate::model::BASE);
        let hint = Cell::new(0u64);
        let mut chunks = 0usize;
        let mut size_at_first_chunk = 0u64;
        let mut meta = |n: u64| hint.set(n);
        let mut sink = |buf: &[u8]| -> bool {
            if chunks == 0 {
                size_at_first_chunk = hint.get();
            }
            chunks += 1;
            !buf.is_empty()
        };
        let r = net::fetch_binary_stream(&url, &mut sink, &mut meta)
            .expect("streaming download must succeed");
        println!(
            "content-length={} at_first_chunk={} chunks={} final={}",
            hint.get(),
            size_at_first_chunk,
            chunks,
            r.final_url
        );
        assert!(hint.get() > 0, "server must report Content-Length");
        assert_eq!(
            size_at_first_chunk,
            hint.get(),
            "size must arrive before the first byte"
        );
        assert!(chunks > 1, "body must stream in multiple chunks");
        assert_eq!(r.size, hint.get());
    }

    /// Full round trip: 302 -> plain-HTTP CDN -> temp file -> signature check
    /// -> rename into Downloads. Run with `cargo test -- --ignored`.
    /// The ~1 MB self-test file is removed at the end.
    #[test]
    #[ignore = "requires live network and writes a temporary file to Downloads"]
    fn live_download_writes_a_real_installer() {
        let url = format!("{}/dlservice/download?softid=L118939", crate::model::BASE);
        let path = download_package("MindosShop-download-self-test", &url, 0)
            .expect("installer download must succeed");
        let len = std::fs::metadata(&path).expect("file must exist").len();
        let mut f = File::open(&path).unwrap();
        let mut head = [0u8; 2];
        f.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"MZ", "downloaded file is not a PE executable");
        drop(f);
        println!("downloaded {} bytes -> {}", len, path);
        assert!(len > 10_000, "installer is suspiciously small");
        std::fs::remove_file(&path).unwrap();
        let leftover = std::fs::read_dir(std::env::var("USERPROFILE").unwrap() + "\\Downloads")
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("download-self-test")
            });
        assert!(!leftover, "no .part/temp file should remain");
    }
}
