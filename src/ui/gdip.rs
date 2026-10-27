//! The small slice of GDI+ the meter needs: fonts, text, a rounded pill, premultiplied bitmaps.

use crate::util::wide;
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::UI::WindowsAndMessaging::HICON;

/// 32 bpp premultiplied ARGB, the layout UpdateLayeredWindow expects.
const PIXEL_FORMAT_32BPP_PARGB: i32 = 0x000E_200B;
const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x0026_200A;

/// GDI+ is started for the life of the widget.
pub struct Gdiplus(usize);

impl Gdiplus {
    pub fn start() -> Option<Gdiplus> {
        let input =
            GdiplusStartupInput { GdiplusVersion: 1, DebugEventCallback: 0, SuppressBackgroundThread: 0, SuppressExternalCodecs: 1 };
        let mut token = 0usize;
        let status = unsafe { GdiplusStartup(&mut token, &input, std::ptr::null_mut()) };
        (status == Ok).then_some(Gdiplus(token))
    }
}

impl Drop for Gdiplus {
    fn drop(&mut self) {
        unsafe { GdiplusShutdown(self.0) };
    }
}

pub struct Font(*mut GpFont);

impl Font {
    /// `family` at `px` pixels; falls back to Segoe UI Bold when the family is not installed.
    pub fn new(family: &str, px: f32, bold: bool) -> Font {
        unsafe fn create(family: &str, px: f32, style: i32) -> Option<*mut GpFont> {
            let mut fam: *mut GpFontFamily = std::ptr::null_mut();
            if unsafe { GdipCreateFontFamilyFromName(wide(family).as_ptr(), std::ptr::null_mut(), &mut fam) } != Ok {
                return None;
            }
            let mut font: *mut GpFont = std::ptr::null_mut();
            let status = unsafe { GdipCreateFont(fam, px, style, UnitPixel, &mut font) };
            unsafe { GdipDeleteFontFamily(fam) };
            (status == Ok).then_some(font)
        }
        let style = if bold { FontStyleBold } else { FontStyleRegular };
        let font = unsafe { create(family, px, style).or_else(|| create("Segoe UI", px, FontStyleBold)) };
        Font(font.unwrap_or(std::ptr::null_mut()))
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { GdipDeleteFont(self.0) };
        }
    }
}

/// A GDI+ drawing surface over a bitmap.
pub struct Canvas {
    bitmap: *mut GpBitmap,
    graphics: *mut GpGraphics,
    format: *mut GpStringFormat,
}

impl Canvas {
    /// Draws into caller-owned premultiplied pixels (a DIB section): width * height * 4 bytes.
    pub fn on_pixels(width: i32, height: i32, pixels: *mut u8) -> Option<Canvas> {
        Self::create(width, height, width * 4, PIXEL_FORMAT_32BPP_PARGB, pixels)
    }

    /// Owns its pixels (straight alpha); used for measuring and for the tray icon.
    pub fn owned(width: i32, height: i32) -> Option<Canvas> {
        Self::create(width, height, 0, PIXEL_FORMAT_32BPP_ARGB, std::ptr::null_mut())
    }

    fn create(width: i32, height: i32, stride: i32, format: i32, pixels: *mut u8) -> Option<Canvas> {
        let mut bitmap: *mut GpBitmap = std::ptr::null_mut();
        if unsafe { GdipCreateBitmapFromScan0(width, height, stride, format, pixels, &mut bitmap) } != Ok {
            return None;
        }
        let mut graphics: *mut GpGraphics = std::ptr::null_mut();
        if unsafe { GdipGetImageGraphicsContext(bitmap as *mut GpImage, &mut graphics) } != Ok {
            unsafe { GdipDisposeImage(bitmap as *mut GpImage) };
            return None;
        }
        let mut typographic: *mut GpStringFormat = std::ptr::null_mut();
        let mut format: *mut GpStringFormat = std::ptr::null_mut();
        unsafe {
            GdipStringFormatGetGenericTypographic(&mut typographic);
            GdipCloneStringFormat(typographic, &mut format);
            GdipSetStringFormatFlags(format, StringFormatFlagsNoWrap | StringFormatFlagsNoClip | StringFormatFlagsLineLimit);
            GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias);
            GdipSetTextRenderingHint(graphics, TextRenderingHintAntiAliasGridFit);
            GdipGraphicsClear(graphics, 0);
        }
        Some(Canvas { bitmap, graphics, format })
    }

    pub fn text_width(&self, text: &str, font: &Font) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let text = wide(text);
        let layout = RectF { X: 0.0, Y: 0.0, Width: 0.0, Height: 0.0 };
        let mut bounds = RectF { X: 0.0, Y: 0.0, Width: 0.0, Height: 0.0 };
        unsafe {
            GdipMeasureString(
                self.graphics,
                text.as_ptr(),
                (text.len() - 1) as i32,
                font.0,
                &layout,
                self.format,
                &mut bounds,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
        bounds.Width
    }

    pub fn line_height(&self, font: &Font) -> f32 {
        let mut height = 0.0f32;
        unsafe { GdipGetFontHeight(font.0, self.graphics, &mut height) };
        height
    }

    pub fn draw_text(&self, text: &str, font: &Font, argb: u32, x: f32, y: f32) {
        if text.is_empty() {
            return;
        }
        let text = wide(text);
        let layout = RectF { X: x.round(), Y: y, Width: 0.0, Height: 0.0 };
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        unsafe {
            GdipCreateSolidFill(argb, &mut brush);
            GdipDrawString(self.graphics, text.as_ptr(), (text.len() - 1) as i32, font.0, &layout, self.format, brush as *mut GpBrush);
            GdipDeleteBrush(brush as *mut GpBrush);
        }
    }

    /// A filled, outlined rectangle with rounded corners; `border` 0 draws no outline.
    pub fn rounded_rect(&self, rect: RectF, radius: f32, fill: u32, border: u32) {
        let RectF { X: x, Y: y, Width: width, Height: height } = rect;
        let d = radius * 2.0;
        let mut path: *mut GpPath = std::ptr::null_mut();
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        let mut pen: *mut GpPen = std::ptr::null_mut();
        unsafe {
            GdipCreatePath(FillModeAlternate, &mut path);
            GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
            GdipAddPathArc(path, x + width - d, y, d, d, 270.0, 90.0);
            GdipAddPathArc(path, x + width - d, y + height - d, d, d, 0.0, 90.0);
            GdipAddPathArc(path, x, y + height - d, d, d, 90.0, 90.0);
            GdipClosePathFigure(path);
            GdipCreateSolidFill(fill, &mut brush);
            GdipFillPath(self.graphics, brush as *mut GpBrush, path);
            if border != 0 {
                GdipCreatePen1(border, 1.0, UnitPixel, &mut pen);
                GdipDrawPath(self.graphics, pen, path);
                GdipDeletePen(pen);
            }
            GdipDeleteBrush(brush as *mut GpBrush);
            GdipDeletePath(path);
        }
    }

    pub fn fill_rect(&self, x: f32, y: f32, width: f32, height: f32, argb: u32) {
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        unsafe {
            GdipCreateSolidFill(argb, &mut brush);
            GdipFillRectangle(self.graphics, brush as *mut GpBrush, x, y, width, height);
            GdipDeleteBrush(brush as *mut GpBrush);
        }
    }

    /// Finishes pending drawing so the pixels can be handed to Windows.
    pub fn flush(&self) {
        unsafe { GdipFlush(self.graphics, FlushIntentionSync) };
    }

    pub fn to_icon(&self) -> HICON {
        self.flush();
        let mut icon: HICON = std::ptr::null_mut();
        unsafe { GdipCreateHICONFromBitmap(self.bitmap, &mut icon) };
        icon
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            GdipDeleteStringFormat(self.format);
            GdipDeleteGraphics(self.graphics);
            GdipDisposeImage(self.bitmap as *mut GpImage);
        }
    }
}
