#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(unsafe_op_in_unsafe_fn)]
//! AllStore — a pure native Windows app store client.
//! Single executable, one process, no runtime, no browser, no telemetry.

mod app;
mod downloads;
mod gfx;
mod installed;
mod library;
mod model;
mod net;
mod persist;
mod source;
mod update;
mod updates_ui;

use std::ffi::c_void;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

#[link(name = "user32")]
unsafe extern "system" {
    fn SetProcessDPIAware() -> i32;
}

fn fatal(text: &str) {
    unsafe {
        let _ = MessageBoxW(
            None,
            &HSTRING::from(text),
            w!("AllStore"),
            MB_ICONERROR | MB_OK,
        );
    }
}

/// One-time data migration from the pre-rename layout (`MindosShop`) to the
/// current one (`AllStore`). Copies every `*.json` state file (downloads,
/// urlcheck, mirrors, sources, github, updates) so an upgrade keeps the
/// download queue, dead-link verdicts and update cache intact. Best-effort:
/// any failure simply falls back to a fresh state dir.
fn migrate_legacy_data() {
    use std::path::PathBuf;
    let Some(base) = std::env::var("LOCALAPPDATA").ok() else {
        return;
    };
    let old = PathBuf::from(&base).join("MindosShop");
    let new = PathBuf::from(&base).join("AllStore");
    if !old.is_dir() || new.exists() {
        return;
    }
    if std::fs::create_dir_all(&new).is_err() {
        return;
    }
    let _ = std::fs::read_dir(&old).map(|rd| {
        for e in rd.flatten() {
            if e.path().extension().is_some_and(|x| x == "json") {
                let _ = std::fs::copy(e.path(), new.join(e.file_name()));
            }
        }
    });
    // Stale `.part` fragments live under `%TEMP%\MindosShop`; the old cleanup
    // pass will not run anymore after the rename, so sweep the dir once here.
    let legacy_tmp = std::env::temp_dir().join("MindosShop");
    if legacy_tmp.is_dir() {
        let _ = std::fs::remove_dir_all(legacy_tmp);
    }
}

fn main() {
    migrate_legacy_data();
    unsafe {
        // Prefer per-monitor DPI v2 so a window dragged between monitors keeps
        // crisp text; fall back to the legacy system-DPI API on older Windows.
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
            SetProcessDPIAware();
        }
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let hmod = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let instance = HINSTANCE(hmod.0);
        let class_name = w!("AllStoreWindow");
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(app::wndproc),
            hInstance: instance,
            hCursor: cursor,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            fatal("无法注册窗口类");
            return;
        }

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("AllStore"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            120,
            90,
            1100,
            720,
            None,
            None,
            Some(instance),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                fatal(&format!("无法创建窗口：{}", e));
                return;
            }
        };

        // Fluent window treatment: Mica backdrop + immersive dark mode.
        let dark = gfx::system_dark();
        let backdrop: i32 = 2; // DWMSBT_MAINWINDOW
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(38),
            &backdrop as *const i32 as *const c_void,
            4,
        );
        let dark_value: i32 = if dark { 1 } else { 0 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(20),
            &dark_value as *const i32 as *const c_void,
            4,
        );

        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        let cw = ((r.right - r.left).max(1)) as u32;
        let ch = ((r.bottom - r.top).max(1)) as u32;
        let graphics = match gfx::Gfx::new(hwnd, cw, ch, dark) {
            Ok(g) => g,
            Err(e) => {
                fatal(&format!("图形初始化失败：{}", e));
                return;
            }
        };
        let state = Box::new(app::AppState::new(hwnd, graphics, cw, ch));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        // 500 ms clock: caret blink + transfer-speed sampling for the graph.
        SetTimer(Some(hwnd), app::TIMER_ID, 500, None);
        // Stale partial cleanup + task restore happen in AppState::new, where
        // the live task keys are known (so resumable partials are preserved).
        app::start_load(hwnd.0 as usize);

        let mut msg = MSG::default();
        loop {
            // -1 means GetMessage failed rather than WM_QUIT; spinning on a
            // stale MSG would peg a core at 100%.
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = KillTimer(Some(hwnd), app::TIMER_ID);
        CoUninitialize();
    }
}
