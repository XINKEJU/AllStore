#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(unsafe_op_in_unsafe_fn)]
//! MindosShop — a pure native Windows app store client.
//! Single executable, one process, no runtime, no browser, no telemetry.

mod app;
mod gfx;
mod installed;
mod model;
mod net;
mod update;
mod updates_ui;

use std::ffi::c_void;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::*;
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
            w!("MindosShop"),
            MB_ICONERROR | MB_OK,
        );
    }
}

fn main() {
    unsafe {
        SetProcessDPIAware();
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let hmod = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let instance = HINSTANCE(hmod.0);
        let class_name = w!("MindosShopWindow");
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
            w!("MindosShop"),
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
        CoUninitialize();
    }
}
