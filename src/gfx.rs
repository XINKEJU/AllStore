//! Direct2D / DirectWrite / WIC rendering layer. Everything is drawn with
//! system components: no browser, no skia, no bundled fonts or TLS stack.
use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;

#[derive(Clone, Copy)]
pub struct Color(pub f32, pub f32, pub f32, pub f32);

impl Color {
    fn d2(self) -> D2D1_COLOR_F {
        D2D1_COLOR_F { r: self.0, g: self.1, b: self.2, a: self.3 }
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color,
    pub panel: Color,
    pub card: Color,
    pub hover: Color,
    pub text: Color,
    pub sub: Color,
    pub accent: Color,
    pub on_accent: Color,
    pub border: Color,
    pub status: Color,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bg: Color(0.118, 0.125, 0.145, 1.0),
            panel: Color(0.157, 0.165, 0.188, 1.0),
            card: Color(0.196, 0.204, 0.227, 1.0),
            hover: Color(0.239, 0.267, 0.325, 1.0),
            text: Color(0.957, 0.965, 0.980, 1.0),
            sub: Color(0.647, 0.671, 0.718, 1.0),
            accent: Color(0.298, 0.557, 0.965, 1.0),
            on_accent: Color(1.0, 1.0, 1.0, 1.0),
            border: Color(0.278, 0.290, 0.325, 1.0),
            status: Color(0.141, 0.149, 0.173, 1.0),
        }
    } else {
        Palette {
            bg: Color(0.965, 0.973, 0.984, 1.0),
            panel: Color(1.0, 1.0, 1.0, 1.0),
            card: Color(1.0, 1.0, 1.0, 1.0),
            hover: Color(0.925, 0.953, 1.0, 1.0),
            text: Color(0.106, 0.114, 0.129, 1.0),
            sub: Color(0.420, 0.443, 0.482, 1.0),
            accent: Color(0.184, 0.435, 0.929, 1.0),
            on_accent: Color(1.0, 1.0, 1.0, 1.0),
            border: Color(0.898, 0.910, 0.929, 1.0),
            status: Color(0.933, 0.945, 0.965, 1.0),
        }
    }
}

pub fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

#[allow(dead_code)]
pub struct Gfx {
    // Kept alive so the D2D factory COM object is not released early.
    pub factory: ID2D1Factory,
    pub dwrite: IDWriteFactory,
    pub wic: IWICImagingFactory,
    pub rt: ID2D1HwndRenderTarget,
    formats: HashMap<(u32, bool), IDWriteTextFormat>,
    pub icons: HashMap<String, ID2D1Bitmap>,
    pub pal: Palette,
    /// D2D scales DIPs by system DPI; keep physical size separate from layout.
    pub scale: f32,
    pub phys_w: u32,
    pub phys_h: u32,
}

impl Gfx {
    pub fn new(hwnd: HWND, width: u32, height: u32, dark: bool) -> Result<Self, String> {
        unsafe {
            let factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                    .map_err(|e| format!("D2D1CreateFactory: {}", e))?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
                .map_err(|e| format!("DWriteCreateFactory: {}", e))?;
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("WIC factory: {}", e))?;
            let rt_props = D2D1_RENDER_TARGET_PROPERTIES::default();
            let mut hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES::default();
            hwnd_props.hwnd = hwnd;
            hwnd_props.pixelSize = D2D_SIZE_U { width, height };
            let rt = factory
                .CreateHwndRenderTarget(&rt_props, &hwnd_props)
                .map_err(|e| format!("CreateHwndRenderTarget: {}", e))?;
            let mut dpi_x = 96.0f32;
            let mut dpi_y = 96.0f32;
            let _ = rt.GetDpi(&mut dpi_x, &mut dpi_y);
            let scale = if dpi_x > 1.0 { dpi_x / 96.0 } else { 1.0 };
            Ok(Self {
                factory,
                dwrite,
                wic,
                rt,
                formats: HashMap::new(),
                icons: HashMap::new(),
                pal: palette(dark),
                scale,
                phys_w: width,
                phys_h: height,
            })
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.phys_w = width;
        self.phys_h = height;
        unsafe {
            let _ = self.rt.Resize(&D2D_SIZE_U { width, height });
        }
    }

    fn brush(&self, c: Color) -> Option<ID2D1SolidColorBrush> {
        unsafe { self.rt.CreateSolidColorBrush(&c.d2(), None).ok() }
    }

    fn format(&mut self, size: u32, bold: bool) -> Option<IDWriteTextFormat> {
        if let Some(f) = self.formats.get(&(size, bold)) {
            return Some(f.clone());
        }
        unsafe {
            let weight = if bold {
                DWRITE_FONT_WEIGHT_BOLD
            } else {
                DWRITE_FONT_WEIGHT_NORMAL
            };
            let f = self
                .dwrite
                .CreateTextFormat(
                    w!("Microsoft YaHei UI"),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size as f32,
                    w!("zh-CN"),
                )
                .ok()?;
            self.formats.insert((size, bold), f.clone());
            Some(f)
        }
    }

    pub fn begin(&self) {
        unsafe {
            let c = self.pal.bg.d2();
            let _ = self.rt.BeginDraw();
            let _ = self.rt.Clear(Some(&c as *const D2D1_COLOR_F));
        }
    }

    pub fn end(&self) {
        unsafe {
            let _ = self.rt.EndDraw(None, None);
        }
    }

    pub fn fill(&self, r: D2D_RECT_F, c: Color) {
        unsafe {
            if let Some(b) = self.brush(c) {
                let _ = self.rt.FillRectangle(&r, &b);
            }
        }
    }

    pub fn round(&self, r: D2D_RECT_F, radius: f32, c: Color) {
        unsafe {
            if let Some(b) = self.brush(c) {
                let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
                let _ = self.rt.FillRoundedRectangle(&rr, &b);
            }
        }
    }

    pub fn outline(&self, r: D2D_RECT_F, c: Color, width: f32) {
        unsafe {
            if let Some(b) = self.brush(c) {
                let _ = self.rt.DrawRectangle(&r, &b, width, None);
            }
        }
    }

    pub fn text(&mut self, s: &str, r: D2D_RECT_F, size: u32, bold: bool, c: Color) {
        unsafe {
            let f = match self.format(size, bold) {
                Some(f) => f,
                None => return,
            };
            let b = match self.brush(c) {
                Some(b) => b,
                None => return,
            };
            let w: Vec<u16> = s.encode_utf16().collect();
            let _ = self.rt.DrawText(
                &w,
                &f,
                &r,
                &b,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    pub fn center_text(&mut self, s: &str, r: D2D_RECT_F, size: u32, bold: bool, c: Color) {
        unsafe {
            let f = match self.format(size, bold) {
                Some(f) => f,
                None => return,
            };
            let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let b = match self.brush(c) {
                Some(b) => b,
                None => return,
            };
            let w: Vec<u16> = s.encode_utf16().collect();
            let _ = self.rt.DrawText(
                &w,
                &f,
                &r,
                &b,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
            let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        }
    }

    pub fn bitmap(&self, key: &str, r: D2D_RECT_F) -> bool {
        if let Some(bmp) = self.icons.get(key) {
            unsafe {
                let _ = self.rt.DrawBitmap(
                    bmp,
                    Some(&r as *const D2D_RECT_F),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    None,
                );
            }
            true
        } else {
            false
        }
    }

    /// Decode PNG/JPEG bytes through WIC into a premultiplied BGRA D2D bitmap.
    pub fn decode_icon(&self, bytes: &[u8]) -> Option<ID2D1Bitmap> {
        unsafe {
            let stream = self.wic.CreateStream().ok()?;
            stream.InitializeFromMemory(bytes).ok()?;
            let istream: IStream = stream.cast().ok()?;
            let decoder = self
                .wic
                .CreateDecoderFromStream(
                    &istream,
                    std::ptr::null(),
                    WICDecodeMetadataCacheOnDemand,
                )
                .ok()?;
            let frame = decoder.GetFrame(0).ok()?;
            let frame_src: IWICBitmapSource = frame.cast().ok()?;
            let conv = self.wic.CreateFormatConverter().ok()?;
            conv.Initialize(
                &frame_src,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )
            .ok()?;
            let src: IWICBitmapSource = conv.cast().ok()?;
            let mut width = 0u32;
            let mut height = 0u32;
            src.GetSize(&mut width, &mut height).ok()?;
            if width == 0 || height == 0 || width > 2048 || height > 2048 {
                return None;
            }
            let stride = width * 4;
            let mut buf = vec![0u8; (stride * height) as usize];
            src.CopyPixels(std::ptr::null(), stride, &mut buf).ok()?;
            let props = D2D1_BITMAP_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
            };
            let bmp = self
                .rt
                .CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &props)
                .ok()?;
            bmp.CopyFromMemory(None, buf.as_ptr() as *const c_void, stride)
                .ok()?;
            Some(bmp)
        }
    }
}

/// Read the Windows light/dark preference.
pub fn system_dark() -> bool {
    use windows::Win32::System::Registry::*;
    use windows::core::HSTRING;
    unsafe {
        let mut key = HKEY::default();
        let sub = HSTRING::from(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        );
        if RegOpenKeyExW(HKEY_CURRENT_USER, &sub, None, KEY_READ, &mut key).is_err() {
            return false;
        }
        let name = HSTRING::from("AppsUseLightTheme");
        let mut data: u32 = 1;
        let mut size = std::mem::size_of::<u32>() as u32;
        let mut kind = REG_VALUE_TYPE(0);
        let r = RegQueryValueExW(
            key,
            &name,
            None,
            Some(&mut kind),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        r.is_ok() && data == 0
    }
}

#[allow(dead_code)]
fn _pcwstr_marker() -> PCWSTR {
    PCWSTR::null()
}
