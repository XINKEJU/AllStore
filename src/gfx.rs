//! Direct2D / DirectWrite / WIC rendering layer. Everything is drawn with
//! system components: no browser, no skia, no bundled fonts or TLS stack.
use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::core::{Interface, PCWSTR, w};
use windows_numerics::Vector2;

#[derive(Clone, Copy)]
pub struct Color(pub f32, pub f32, pub f32, pub f32);

impl Color {
    fn d2(self) -> D2D1_COLOR_F {
        D2D1_COLOR_F {
            r: self.0,
            g: self.1,
            b: self.2,
            a: self.3,
        }
    }
    fn key(self) -> u32 {
        self.0.to_bits()
            ^ self.1.to_bits().rotate_left(8)
            ^ self.2.to_bits().rotate_left(16)
            ^ self.3.to_bits().rotate_left(24)
    }
}

impl Palette {
    fn all(self) -> [Color; 17] {
        [
            self.bg,
            self.panel,
            self.card,
            self.hover,
            self.text,
            self.sub,
            self.accent,
            self.on_accent,
            self.border,
            self.status,
            self.accent_soft,
            self.subtle,
            self.danger,
            self.net,
            self.disk,
            self.net_line,
            self.disk_line,
        ]
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
    pub accent_soft: Color,
    pub subtle: Color,
    pub on_accent: Color,
    pub border: Color,
    pub status: Color,
    pub danger: Color,
    /// Transfer graph: translucent bars for each instantaneous sample and
    /// solid lines for the smoothed trend. Network = blue, disk = teal.
    pub net: Color,
    pub disk: Color,
    pub net_line: Color,
    pub disk_line: Color,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bg: Color(0.098, 0.106, 0.129, 1.0),
            panel: Color(0.137, 0.149, 0.176, 1.0),
            card: Color(0.169, 0.184, 0.216, 1.0),
            hover: Color(0.216, 0.243, 0.298, 1.0),
            text: Color(0.961, 0.969, 0.980, 1.0),
            sub: Color(0.616, 0.651, 0.706, 1.0),
            accent: Color(0.353, 0.604, 1.0, 1.0),
            accent_soft: Color(0.196, 0.282, 0.424, 1.0),
            subtle: Color(0.239, 0.259, 0.302, 1.0),
            on_accent: Color(1.0, 1.0, 1.0, 1.0),
            border: Color(0.243, 0.267, 0.310, 1.0),
            status: Color(0.118, 0.129, 0.157, 1.0),
            danger: Color(0.976, 0.447, 0.447, 1.0),
            net: Color(0.353, 0.604, 1.0, 0.46),
            disk: Color(0.290, 0.855, 0.710, 0.34),
            net_line: Color(0.529, 0.741, 1.0, 1.0),
            disk_line: Color(0.400, 0.929, 0.780, 1.0),
        }
    } else {
        Palette {
            bg: Color(0.957, 0.965, 0.976, 1.0),
            panel: Color(0.992, 0.996, 1.0, 1.0),
            card: Color(1.0, 1.0, 1.0, 1.0),
            hover: Color(0.929, 0.953, 0.992, 1.0),
            text: Color(0.094, 0.114, 0.149, 1.0),
            sub: Color(0.427, 0.463, 0.518, 1.0),
            accent: Color(0.145, 0.388, 0.898, 1.0),
            accent_soft: Color(0.882, 0.922, 0.992, 1.0),
            subtle: Color(0.878, 0.902, 0.933, 1.0),
            on_accent: Color(1.0, 1.0, 1.0, 1.0),
            border: Color(0.886, 0.906, 0.933, 1.0),
            status: Color(0.933, 0.949, 0.969, 1.0),
            danger: Color(0.843, 0.196, 0.224, 1.0),
            net: Color(0.145, 0.388, 0.898, 0.42),
            disk: Color(0.055, 0.647, 0.545, 0.34),
            net_line: Color(0.098, 0.298, 0.780, 1.0),
            disk_line: Color(0.020, 0.478, 0.404, 1.0),
        }
    }
}

pub fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

/// A cached DirectWrite layout. Building a layout is by far the most expensive
/// part of drawing a label (font fallback, shaping, line breaking). Rendering
/// the same static text on every mouse move used to rebuild all of them; now
/// each distinct (text, width, style, clamp) tuple is shaped exactly once.
#[derive(Clone, Hash, PartialEq, Eq)]
struct LayoutKey {
    text: String,
    size: u32,
    bold: bool,
    center: bool,
    wrap: bool,
    /// Width in half DIPs, so the key stays hashable.
    width: u32,
    /// 0 = unlimited; otherwise the number of visible lines.
    lines: u32,
}

#[derive(Clone)]
struct TextLayout {
    layout: IDWriteTextLayout,
    height: f32,
    width: f32,
    line_h: f32,
}

impl LayoutKey {
    fn new(
        text: &str,
        width: f32,
        size: u32,
        bold: bool,
        center: bool,
        wrap: bool,
        lines: u32,
    ) -> Self {
        Self {
            text: text.to_string(),
            size,
            bold,
            center,
            wrap,
            width: (width.max(1.0) * 2.0).round() as u32,
            lines,
        }
    }
    fn layout_width(&self) -> f32 {
        (self.width as f32 * 0.5).max(1.0)
    }
}

#[allow(dead_code)]
pub struct Gfx {
    // Kept alive so the D2D factory COM object is not released early.
    pub factory: ID2D1Factory,
    pub dwrite: IDWriteFactory,
    pub wic: IWICImagingFactory,
    pub rt: ID2D1HwndRenderTarget,
    hwnd: HWND,
    /// Alignment is part of the key: `center_text` used to mutate a shared
    /// text format, which left every later label misaligned on early returns.
    formats: HashMap<(u32, bool, bool), IDWriteTextFormat>,
    /// Palette brushes are created once instead of once per drawn primitive.
    brushes: HashMap<u32, ID2D1SolidColorBrush>,
    layouts: HashMap<LayoutKey, TextLayout>,
    ellipsis: HashMap<(u32, bool), IDWriteInlineObject>,
    line_heights: HashMap<(u32, bool), f32>,
    pub icons: HashMap<String, ID2D1Bitmap>,
    /// Insertion order for a real LRU instead of dropping every icon at once.
    icon_order: Vec<String>,
    pub pal: Palette,
    /// D2D scales DIPs by system DPI; keep physical size separate from layout.
    pub scale: f32,
    dpi: f32,
    pub phys_w: u32,
    pub phys_h: u32,
    dark: bool,
}

impl Gfx {
    pub fn new(hwnd: HWND, width: u32, height: u32, dark: bool) -> Result<Self, String> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                .map_err(|e| format!("D2D1CreateFactory: {}", e))?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
                .map_err(|e| format!("DWriteCreateFactory: {}", e))?;
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("WIC factory: {}", e))?;
            let rt_props = D2D1_RENDER_TARGET_PROPERTIES::default();
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U { width, height },
                ..Default::default()
            };
            let rt = factory
                .CreateHwndRenderTarget(&rt_props, &hwnd_props)
                .map_err(|e| format!("CreateHwndRenderTarget: {}", e))?;
            let mut dpi_x = 96.0f32;
            let mut dpi_y = 96.0f32;
            // A per-monitor-v2 window does not inherit the system DPI into a
            // D2D HWND target: without this, text renders 1:1 pixel on a
            // 125%/150% display and looks tiny.
            let window_dpi = GetDpiForWindow(hwnd);
            if window_dpi >= 96 {
                dpi_x = window_dpi as f32;
                dpi_y = window_dpi as f32;
                rt.SetDpi(dpi_x, dpi_y);
            }
            rt.GetDpi(&mut dpi_x, &mut dpi_y);
            let scale = if dpi_x > 1.0 { dpi_x / 96.0 } else { 1.0 };
            let pal = palette(dark);
            let mut brushes = HashMap::new();
            for c in pal.all() {
                if let Ok(b) = rt.CreateSolidColorBrush(&c.d2(), None) {
                    brushes.insert(c.key(), b);
                }
            }
            Ok(Self {
                factory,
                dwrite,
                wic,
                rt,
                hwnd,
                formats: HashMap::new(),
                brushes,
                layouts: HashMap::with_capacity(256),
                ellipsis: HashMap::new(),
                line_heights: HashMap::new(),
                icons: HashMap::new(),
                icon_order: Vec::new(),
                pal,
                scale,
                dpi: dpi_x,
                phys_w: width,
                phys_h: height,
                dark,
            })
        }
    }

    /// Rebuild GPU resources after D2DERR_RECREATE_TARGET (driver reset, RDP
    /// reconnect, monitor change). Without this the window turns black forever.
    pub fn recreate(&mut self) {
        unsafe {
            let props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd: self.hwnd,
                pixelSize: D2D_SIZE_U {
                    width: self.phys_w.max(1),
                    height: self.phys_h.max(1),
                },
                ..Default::default()
            };
            let Ok(rt) = self
                .factory
                .CreateHwndRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES::default(), &props)
            else {
                return;
            };
            self.rt = rt;
            let mut dpi_x = self.dpi;
            let mut dpi_y = self.dpi;
            self.rt.SetDpi(dpi_x, dpi_y);
            self.rt.GetDpi(&mut dpi_x, &mut dpi_y);
            self.scale = if dpi_x > 1.0 { dpi_x / 96.0 } else { 1.0 };
            self.pal = palette(self.dark);
            self.brushes.clear();
            for c in self.pal.all() {
                if let Ok(b) = self.rt.CreateSolidColorBrush(&c.d2(), None) {
                    self.brushes.insert(c.key(), b);
                }
            }
            // Bitmaps and layouts belong to the old target / factory state.
            self.icons.clear();
            self.icon_order.clear();
            self.layouts.clear();
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.phys_w = width;
        self.phys_h = height;
        unsafe {
            let _ = self.rt.Resize(&D2D_SIZE_U { width, height });
        }
    }

    /// Keep DIP layout in sync when the window moves to a monitor with a
    /// different scale factor (per-monitor DPI v2).
    pub fn set_dpi(&mut self, dpi_x: f32, dpi_y: f32) {
        if dpi_x > 1.0 && dpi_y > 1.0 {
            unsafe { self.rt.SetDpi(dpi_x, dpi_y) };
            self.dpi = dpi_x;
            self.scale = dpi_x / 96.0;
            self.layouts.clear();
        }
    }

    /// Borrowed brush: no AddRef/Release per rectangle any more.
    fn brush(&self, c: Color) -> Option<&ID2D1SolidColorBrush> {
        self.brushes.get(&c.key())
    }

    /// Insert an icon with a bounded LRU. Previously all 200 cached bitmaps
    /// were dropped at once, so scrolling re-downloaded every logo.
    pub fn put_icon(&mut self, key: String, bmp: ID2D1Bitmap) {
        const LIMIT: usize = 160;
        if self.icon_order.len() >= LIMIT {
            let drop: Vec<String> = self.icon_order.drain(..40).collect();
            for k in drop {
                self.icons.remove(&k);
            }
        }
        self.icon_order.push(key.clone());
        self.icons.insert(key, bmp);
    }

    fn format(&mut self, size: u32, bold: bool, center: bool) -> Option<IDWriteTextFormat> {
        let key = (size, bold, center);
        if let Some(f) = self.formats.get(&key) {
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
            if center {
                let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
                let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            }
            self.formats.insert(key, f.clone());
            Some(f)
        }
    }

    pub fn begin(&self) {
        unsafe {
            let c = self.pal.bg.d2();
            self.rt.BeginDraw();
            self.rt.Clear(Some(&c as *const D2D1_COLOR_F));
        }
    }

    pub fn end(&self) -> bool {
        unsafe { self.rt.EndDraw(None, None).is_ok() }
    }

    /// Wire the device-loss recovery into the frame loop.
    pub fn present(&mut self) {
        if !self.end() {
            self.recreate();
        }
    }

    pub fn fill(&self, r: D2D_RECT_F, c: Color) {
        if let Some(b) = self.brush(c) {
            unsafe { self.rt.FillRectangle(&r, b) };
        }
    }

    pub fn round(&self, r: D2D_RECT_F, radius: f32, c: Color) {
        if let Some(b) = self.brush(c) {
            unsafe {
                let rr = D2D1_ROUNDED_RECT {
                    rect: r,
                    radiusX: radius,
                    radiusY: radius,
                };
                self.rt.FillRoundedRectangle(&rr, b);
            }
        }
    }

    /// Straight line segment, used by the transfer-speed trend graph.
    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, c: Color, width: f32) {
        if let Some(b) = self.brush(c) {
            unsafe {
                self.rt.DrawLine(
                    Vector2 { X: x0, Y: y0 },
                    Vector2 { X: x1, Y: y1 },
                    b,
                    width,
                    None,
                );
            }
        }
    }

    pub fn outline(&self, r: D2D_RECT_F, c: Color, width: f32) {
        if let Some(b) = self.brush(c) {
            unsafe { self.rt.DrawRectangle(&r, b, width, None) };
        }
    }

    /// Clip scrolling content to the content pane so long descriptions can
    /// never paint over the header, navigation or status bar.
    pub fn push_clip(&self, r: D2D_RECT_F) {
        unsafe {
            self.rt.PushAxisAlignedClip(&r, D2D1_ANTIALIAS_MODE_ALIASED);
        }
    }

    pub fn pop_clip(&self) {
        unsafe {
            self.rt.PopAxisAlignedClip();
        }
    }

    /// Exact line height for a style, measured with a mixed Latin/CJK probe so
    /// font fallback metrics are included.
    pub fn line_h(&mut self, size: u32, bold: bool) -> f32 {
        if let Some(&h) = self.line_heights.get(&(size, bold)) {
            return h;
        }
        let fallback = size as f32 * 1.35;
        let h = (|| {
            let f = self.format(size, bold, false)?;
            let probe: Vec<u16> = "Ag中文".encode_utf16().collect();
            let l = unsafe {
                self.dwrite
                    .CreateTextLayout(&probe, &f, 10000.0, 10000.0)
                    .ok()?
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            unsafe { l.GetMetrics(&mut m).ok()? };
            Some(m.height.max(1.0))
        })()
        .unwrap_or(fallback);
        self.line_heights.insert((size, bold), h);
        h
    }

    fn ellipsis_sign(&mut self, size: u32, bold: bool) -> Option<IDWriteInlineObject> {
        if let Some(s) = self.ellipsis.get(&(size, bold)) {
            return Some(s.clone());
        }
        let f = self.format(size, bold, false)?;
        let sign = unsafe { self.dwrite.CreateEllipsisTrimmingSign(&f).ok()? };
        self.ellipsis.insert((size, bold), sign.clone());
        Some(sign)
    }

    /// Shape text once and reuse it. `wrap = false` guarantees a single line
    /// with a real ellipsis; `wrap = true` uses DirectWrite's own line breaker
    /// and clips to `lines` with an ellipsis on the last visible line.
    fn layout_for(&mut self, key: LayoutKey) -> Option<TextLayout> {
        if let Some(t) = self.layouts.get(&key) {
            return Some(t.clone());
        }
        let width = key.layout_width();
        let f = self.format(key.size, key.bold, false)?;
        let sign = self.ellipsis_sign(key.size, key.bold)?;
        let lh = self.line_h(key.size, key.bold);
        let text: Vec<u16> = key.text.encode_utf16().collect();
        let max_h = if key.wrap && key.lines > 0 {
            lh * key.lines as f32
        } else {
            1_000_000.0
        };
        let l = unsafe { self.dwrite.CreateTextLayout(&text, &f, width, max_h).ok()? };
        let tl = unsafe {
            let _ = l.SetWordWrapping(if key.wrap {
                DWRITE_WORD_WRAPPING_WRAP
            } else {
                DWRITE_WORD_WRAPPING_NO_WRAP
            });
            if key.center {
                let _ = l.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
            }
            let trimming = DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: 0,
                delimiterCount: 0,
            };
            let _ = l.SetTrimming(&trimming, &sign);
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = l.GetMetrics(&mut m);
            TextLayout {
                layout: l,
                height: m.height.max(0.0),
                width: m.width,
                line_h: lh,
            }
        };
        // Bounded cache: a generational clear is cheap and avoids unbounded
        // growth from status strings that change on every progress tick.
        if self.layouts.len() >= 1024 {
            self.layouts.clear();
        }
        self.layouts.insert(key, tl.clone());
        Some(tl)
    }

    /// Exact rendered width of a single line (used to size chips/badges).
    pub fn measure_width(&mut self, s: &str, size: u32, bold: bool) -> f32 {
        let key = LayoutKey::new(s, 100_000.0, size, bold, false, false, 0);
        self.layout_for(key).map(|t| t.width).unwrap_or(0.0)
    }

    /// Exact rendered height of wrapped text, before drawing anything.
    pub fn measure_height(
        &mut self,
        s: &str,
        width: f32,
        size: u32,
        bold: bool,
        lines: u32,
    ) -> f32 {
        let key = LayoutKey::new(s, width, size, bold, false, true, lines);
        self.layout_for(key).map(|t| t.height).unwrap_or(0.0)
    }

    fn draw_layout(&self, t: &TextLayout, x: f32, y: f32, c: Color) {
        let Some(b) = self.brush(c) else { return };
        unsafe {
            self.rt.DrawTextLayout(
                Vector2 { X: x, Y: y },
                &t.layout,
                b,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
            );
        }
    }

    /// Single-line label. Never wraps, never overlaps the next element, and
    /// always ends in a real ellipsis when it does not fit.
    pub fn text(&mut self, s: &str, r: D2D_RECT_F, size: u32, bold: bool, c: Color) {
        self.label(s, r, size, bold, c, false);
    }

    pub fn center_text(&mut self, s: &str, r: D2D_RECT_F, size: u32, bold: bool, c: Color) {
        self.label(s, r, size, bold, c, true);
    }

    pub fn label(&mut self, s: &str, r: D2D_RECT_F, size: u32, bold: bool, c: Color, center: bool) {
        if s.is_empty() || r.right <= r.left || r.bottom <= r.top {
            return;
        }
        let key = LayoutKey::new(s, r.right - r.left, size, bold, center, false, 0);
        let Some(t) = self.layout_for(key) else {
            return;
        };
        let y = r.top + ((r.bottom - r.top) - t.line_h).max(0.0) * 0.5;
        self.draw_layout(&t, r.left, y, c);
    }

    /// Multi-line paragraph. DirectWrite performs the wrapping, so the height
    /// returned is the truth — callers can reserve exactly that much space and
    /// the following section can never be painted underneath the text.
    pub fn paragraph(
        &mut self,
        s: &str,
        r: D2D_RECT_F,
        size: u32,
        bold: bool,
        c: Color,
        lines: u32,
    ) -> f32 {
        let width = r.right - r.left;
        if s.is_empty() || width <= 0.0 {
            return 0.0;
        }
        let key = LayoutKey::new(s, width, size, bold, false, true, lines);
        let Some(t) = self.layout_for(key) else {
            return 0.0;
        };
        let h = t.height;
        self.draw_layout(&t, r.left, r.top, c);
        h
    }

    pub fn bitmap(&self, key: &str, r: D2D_RECT_F) -> bool {
        if let Some(bmp) = self.icons.get(key) {
            unsafe {
                self.rt.DrawBitmap(
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
                .CreateDecoderFromStream(&istream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
                .ok()?;
            let frame = decoder.GetFrame(0).ok()?;
            let frame_src: IWICBitmapSource = frame.cast().ok()?;
            let mut raw_w = 0u32;
            let mut raw_h = 0u32;
            frame_src.GetSize(&mut raw_w, &mut raw_h).ok()?;
            if raw_w == 0 || raw_h == 0 || raw_w > 4096 || raw_h > 4096 {
                return None;
            }
            // Icons are displayed at 48-96 DIPs. Downscale before uploading to
            // the GPU so a 2048x2048 GIF cannot eat 16 MB of VRAM.
            const MAX: u32 = 128;
            let (tw, th) = if raw_w.max(raw_h) > MAX {
                let s = MAX as f32 / raw_w.max(raw_h) as f32;
                (
                    (raw_w as f32 * s).ceil() as u32,
                    (raw_h as f32 * s).ceil() as u32,
                )
            } else {
                (raw_w, raw_h)
            };
            let scaled: Option<IWICBitmapSource> = if (tw, th) != (raw_w, raw_h) {
                let scaler = self.wic.CreateBitmapScaler().ok()?;
                scaler
                    .Initialize(
                        &frame_src,
                        tw.max(1),
                        th.max(1),
                        WICBitmapInterpolationModeFant,
                    )
                    .ok()?;
                Some(scaler.cast().ok()?)
            } else {
                None
            };
            let source: &IWICBitmapSource = scaled.as_ref().unwrap_or(&frame_src);
            let conv = self.wic.CreateFormatConverter().ok()?;
            conv.Initialize(
                source,
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
            if width == 0 || height == 0 {
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
        let sub = HSTRING::from(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Themes\Personalize");
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
