mod fluent;

use std::{
    cell::{OnceCell, RefCell},
    ptr::null_mut,
};

use windows_sys::{
    Win32::{
        Foundation::{COLORREF, RECT},
        Graphics::{Gdi::HDC, GdiPlus::*},
    },
    core::w,
};

#[derive(Default)]
pub(super) struct Renderer {
    // Release the cached GDI+ paths and fonts before shutting down GDI+.
    text: OnceCell<Option<TextResources>>,
    glyphs: [OnceCell<Option<GlyphPath>>; 7],
    session: OnceCell<Option<Session>>,
}

struct Session(usize);

impl Session {
    fn open() -> Option<Self> {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token = 0;
        let status = unsafe { GdiplusStartup(&mut token, &input, null_mut()) };
        (status == Ok).then(|| Self(token))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                GdiplusShutdown(self.0);
            }
        }
    }
}

impl Renderer {
    pub(super) fn canvas(&self, dc: HDC) -> Option<Canvas<'_>> {
        if self.session.get_or_init(Session::open).is_none() {
            return None;
        }
        let mut graphics = null_mut();
        unsafe {
            if GdipCreateFromHDC(dc, &mut graphics) != Ok {
                return None;
            }
            GdipSetPageUnit(graphics, UnitPixel);
            GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias8x8);
            GdipSetPixelOffsetMode(graphics, PixelOffsetModeHighQuality);
            GdipSetTextRenderingHint(graphics, TextRenderingHintAntiAlias);
        }
        Some(Canvas {
            graphics,
            renderer: self,
        })
    }
}

pub(super) struct Canvas<'a> {
    graphics: *mut GpGraphics,
    renderer: &'a Renderer,
}

impl Canvas<'_> {
    pub(super) fn text(&self, bounds: RECT, value: &[u16], size: f32, ink: COLORREF) -> bool {
        let bounds = RectF {
            X: bounds.left as f32,
            Y: bounds.top as f32,
            Width: (bounds.right - bounds.left) as f32,
            Height: (bounds.bottom - bounds.top) as f32,
        };
        if bounds.Width <= 0.0 || bounds.Height <= 0.0 || value.is_empty() {
            return true;
        }
        let Some(resources) = self.renderer.text.get_or_init(TextResources::new) else {
            return false;
        };
        let Some(font) = resources.font(size) else {
            return false;
        };
        let format = resources.format.0;
        unsafe {
            let mut brush = null_mut();
            if GdipCreateSolidFill(argb(ink), &mut brush) != Ok {
                return false;
            }
            let status = GdipDrawString(
                self.graphics,
                value.as_ptr(),
                value.len() as i32,
                font,
                &bounds,
                format,
                brush.cast(),
            );
            GdipDeleteBrush(brush.cast());
            status == Ok
        }
    }

    pub(super) fn rounded(
        &self,
        bounds: RECT,
        radius: f32,
        face: COLORREF,
        edge: Option<COLORREF>,
    ) {
        let inset = if edge.is_some() { 0.6 } else { 0.0 };
        self.rounded_path(bounds, radius, inset, face, edge);
    }

    pub(super) fn rounded_frame(
        &self,
        bounds: RECT,
        radius: f32,
        thickness: f32,
        face: COLORREF,
        edge: COLORREF,
    ) {
        self.rounded_path(bounds, radius, 0.0, edge, None);
        self.rounded_path(bounds, (radius - thickness).max(0.0), thickness, face, None);
    }

    fn rounded_path(
        &self,
        bounds: RECT,
        radius: f32,
        inset: f32,
        face: COLORREF,
        edge: Option<COLORREF>,
    ) {
        let x = bounds.left as f32 + inset;
        let y = bounds.top as f32 + inset;
        let right = bounds.right as f32 - inset;
        let bottom = bounds.bottom as f32 - inset;
        let width = right - x;
        let height = bottom - y;
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let diameter = (radius * 2.0).min(width).min(height);
        unsafe {
            let mut path = null_mut();
            if GdipCreatePath(FillModeAlternate, &mut path) != Ok {
                return;
            }
            if diameter > 0.0 {
                for (left, top, angle) in [
                    (x, y, 180.0),
                    (right - diameter, y, 270.0),
                    (right - diameter, bottom - diameter, 0.0),
                    (x, bottom - diameter, 90.0),
                ] {
                    GdipAddPathArc(path, left, top, diameter, diameter, angle, 90.0);
                }
                GdipClosePathFigure(path);
            } else {
                GdipAddPathRectangle(path, x, y, width, height);
            }
            let mut brush = null_mut();
            if GdipCreateSolidFill(argb(face), &mut brush) == Ok {
                GdipFillPath(self.graphics, brush.cast(), path);
                GdipDeleteBrush(brush.cast());
            }
            if let Some(pen) = edge.and_then(|color| Pen::new(color, 1.0)) {
                GdipDrawPath(self.graphics, pen.0, path);
            }
            GdipDeletePath(path);
        }
    }

    pub(super) fn symbol(&self, symbol: Symbol, bounds: RECT, size: f32, ink: COLORREF) {
        let (points, optical_center_x, [width_factor, height_factor]) = symbol.outline();
        let Some(path) =
            self.renderer.glyphs[symbol as usize].get_or_init(|| GlyphPath::new(points))
        else {
            return;
        };
        let scale_x = size * width_factor / 20.0;
        let scale_y = size * height_factor / 20.0;
        let x = (bounds.left + bounds.right) as f32 * 0.5 - optical_center_x * scale_x;
        let y = (bounds.top + bounds.bottom) as f32 * 0.5 - 10.0 * scale_y;
        unsafe {
            let mut brush = null_mut();
            if GdipCreateSolidFill(argb(ink), &mut brush) != Ok {
                return;
            }
            let mut state = 0;
            if GdipSaveGraphics(self.graphics, &mut state) == Ok {
                GdipScaleWorldTransform(self.graphics, scale_x, scale_y, MatrixOrderAppend);
                GdipTranslateWorldTransform(self.graphics, x, y, MatrixOrderAppend);
                GdipFillPath(self.graphics, brush.cast(), path.0);
                GdipRestoreGraphics(self.graphics, state);
            }
            GdipDeleteBrush(brush.cast());
        }
    }
}

struct GlyphPath(*mut GpPath);

impl GlyphPath {
    fn new(source: &[fluent::Point]) -> Option<Self> {
        let points = source
            .iter()
            .map(|&(x, y, _)| PointF { X: x, Y: y })
            .collect::<Vec<_>>();
        let types = source.iter().map(|&(_, _, kind)| kind).collect::<Vec<_>>();
        let mut path = null_mut();
        let status = unsafe {
            GdipCreatePath2(
                points.as_ptr(),
                types.as_ptr(),
                points.len() as i32,
                FillModeWinding,
                &mut path,
            )
        };
        (status == Ok).then(|| Self(path))
    }
}

impl Drop for GlyphPath {
    fn drop(&mut self) {
        unsafe {
            GdipDeletePath(self.0);
        }
    }
}

struct TextResources {
    fonts: RefCell<Vec<(f32, Font)>>,
    family: Family,
    format: Format,
}

impl TextResources {
    fn new() -> Option<Self> {
        let mut family = null_mut();
        if unsafe { GdipCreateFontFamilyFromName(w!("Segoe UI"), null_mut(), &mut family) } != Ok {
            return None;
        }
        let family = Family(family);
        Some(Self {
            fonts: RefCell::default(),
            family,
            format: Format::new()?,
        })
    }

    fn font(&self, size: f32) -> Option<*mut GpFont> {
        let mut fonts = self.fonts.borrow_mut();
        if let Some(index) = fonts.iter().position(|(cached, _)| *cached == size) {
            let entry = fonts.remove(index);
            let handle = entry.1.0;
            fonts.push(entry);
            return Some(handle);
        }
        let mut font = null_mut();
        if unsafe { GdipCreateFont(self.family.0, size, FontStyleRegular, UnitPixel, &mut font) }
            != Ok
        {
            return None;
        }
        if fonts.len() == 9 {
            fonts.remove(0);
        }
        fonts.push((size, Font(font)));
        Some(font)
    }
}

struct Family(*mut GpFontFamily);

impl Drop for Family {
    fn drop(&mut self) {
        unsafe {
            GdipDeleteFontFamily(self.0);
        }
    }
}

struct Font(*mut GpFont);

impl Drop for Font {
    fn drop(&mut self) {
        unsafe {
            GdipDeleteFont(self.0);
        }
    }
}

struct Format(*mut GpStringFormat);

impl Format {
    fn new() -> Option<Self> {
        let mut format = null_mut();
        unsafe {
            if GdipCreateStringFormat(StringFormatFlagsNoWrap, 0, &mut format) != Ok {
                return None;
            }
            GdipSetStringFormatLineAlign(format, StringAlignmentCenter);
            GdipSetStringFormatTrimming(format, StringTrimmingEllipsisCharacter);
        }
        Some(Self(format))
    }
}

impl Drop for Format {
    fn drop(&mut self) {
        unsafe {
            GdipDeleteStringFormat(self.0);
        }
    }
}

impl Drop for Canvas<'_> {
    fn drop(&mut self) {
        unsafe {
            GdipDeleteGraphics(self.graphics);
        }
    }
}

struct Pen(*mut GpPen);

impl Pen {
    fn new(color: COLORREF, width: f32) -> Option<Self> {
        let mut pen = null_mut();
        unsafe {
            if GdipCreatePen1(argb(color), width, UnitPixel, &mut pen) != Ok {
                return None;
            }
            GdipSetPenLineCap197819(pen, LineCapRound, LineCapRound, DashCapRound);
            GdipSetPenLineJoin(pen, LineJoinRound);
        }
        Some(Self(pen))
    }
}

impl Drop for Pen {
    fn drop(&mut self) {
        unsafe {
            GdipDeletePen(self.0);
        }
    }
}

fn argb(color: COLORREF) -> u32 {
    let [red, green, blue, _] = color.to_le_bytes();
    u32::from_be_bytes([255, red, green, blue])
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(super) enum Symbol {
    Search,
    Open,
    Shield,
    Folder,
    Copy,
    Formula,
    Application,
}

impl Symbol {
    fn outline(self) -> (&'static [fluent::Point], f32, [f32; 2]) {
        // Match the action silhouettes optically around the same hitbox center.
        match self {
            Self::Open => (fluent::PLAY, 9.5, [1.07, 1.0]),
            Self::Shield => (fluent::SHIELD, 10.0, [1.01, 0.9]),
            Self::Folder => (fluent::FOLDER_OPEN, 10.35, [0.85, 0.98]),
            Self::Search => (fluent::SEARCH, 10.0, [1.0, 1.0]),
            Self::Copy => (fluent::COPY, 10.0, [1.0, 1.0]),
            Self::Formula => (fluent::CALCULATOR, 10.0, [1.0, 1.0]),
            Self::Application => (fluent::WINDOW, 10.0, [1.0, 1.0]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::{Win32::Graphics::Gdi::*, core::w};

    use super::super::view::{APP_ACTION_SIZE, APP_ACTION_SYMBOL, Geometry, overlay_font};

    #[test]
    fn surfaces_icons_and_text_have_antialiased_pixels_at_common_dpis() {
        const SIZE: i32 = 96;
        let renderer = Renderer::default();
        for dpi in [96, 120, 144, 192] {
            unsafe {
                let dc = CreateCompatibleDC(null_mut());
                assert!(!dc.is_null());
                let bitmap = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: SIZE,
                        biHeight: -SIZE,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut data = null_mut();
                let dib = CreateDIBSection(dc, &bitmap, DIB_RGB_COLORS, &mut data, null_mut(), 0);
                assert!(!dib.is_null());
                let previous = SelectObject(dc, dib);
                assert!(!previous.is_null());
                let scale = dpi as f32 / 96.0;
                let bounds = RECT {
                    left: 8,
                    top: 8,
                    right: 80,
                    bottom: 80,
                };
                let antialiased = |draw: &dyn Fn(&Canvas)| {
                    PatBlt(dc, 0, 0, SIZE, SIZE, BLACKNESS);
                    {
                        let canvas = renderer.canvas(dc).expect("GDI+ memory graphics");
                        draw(&canvas);
                    }
                    GdiFlush();
                    let pixels =
                        std::slice::from_raw_parts(data.cast::<u32>(), (SIZE * SIZE) as usize);
                    pixels.iter().any(|&pixel| {
                        let value = pixel & 0x00ff_ffff;
                        value != 0 && value != 0x00ff_ffff
                    })
                };
                let shape = antialiased(&|canvas| {
                    canvas.rounded(bounds, 11.0 * scale, 0x00ff_ffff, None);
                });
                antialiased(&|canvas| {
                    canvas.rounded_frame(bounds, 16.0 * scale, scale, 0, 0x00ff_ffff);
                });
                let outline_pixels =
                    std::slice::from_raw_parts(data.cast::<u32>(), (SIZE * SIZE) as usize);
                let center = (bounds.left + bounds.right) / 2;
                let edge_coverage = [0, 1, 2].map(|offset| {
                    outline_pixels[((bounds.top + offset) * SIZE + center) as usize] & 0xff
                });
                let corner_size = (16.0 * scale).round() as i32;
                let outline_corners = [
                    (bounds.left, bounds.top),
                    (bounds.right - corner_size, bounds.top),
                    (bounds.left, bounds.bottom - corner_size),
                    (bounds.right - corner_size, bounds.bottom - corner_size),
                ]
                .into_iter()
                .all(|(left, top)| {
                    (top + 2..top + corner_size - 2).any(|y| {
                        (left + 2..left + corner_size - 2).any(|x| {
                            (1..255).contains(&(outline_pixels[(y * SIZE + x) as usize] & 0xff))
                        })
                    })
                });
                let glyphs = [
                    (Symbol::Search, 23.0),
                    (Symbol::Copy, 21.0),
                    (Symbol::Formula, 23.0),
                    (Symbol::Application, 28.0),
                ]
                .map(|(symbol, dip)| {
                    antialiased(&|canvas| {
                        canvas.symbol(symbol, bounds, dip * scale, 0x00ff_ffff);
                    })
                });
                let button_size = Geometry {
                    dpi,
                    ..Geometry::default()
                }
                .px(APP_ACTION_SIZE);
                let button_bounds = RECT {
                    left: bounds.left,
                    top: bounds.top,
                    right: bounds.left + button_size,
                    bottom: bounds.top + button_size,
                };
                let actions = [Symbol::Open, Symbol::Shield, Symbol::Folder].map(|symbol| {
                    PatBlt(dc, 0, 0, SIZE, SIZE, BLACKNESS);
                    renderer.canvas(dc).expect("GDI+ memory graphics").symbol(
                        symbol,
                        button_bounds,
                        APP_ACTION_SYMBOL * scale,
                        0x00ff_ffff,
                    );
                    GdiFlush();
                    let pixels =
                        std::slice::from_raw_parts(data.cast::<u32>(), (SIZE * SIZE) as usize);
                    let mut min_x = SIZE;
                    let mut max_x = -1;
                    let mut min_y = SIZE;
                    let mut max_y = -1;
                    let mut solid = 0;
                    let mut smoothed = false;
                    for (index, &pixel) in pixels.iter().enumerate() {
                        let ink = pixel & 0xff;
                        solid += i32::from(ink == 255);
                        smoothed |= (1..255).contains(&ink);
                        if ink < 64 {
                            continue;
                        }
                        let x = index as i32 % SIZE;
                        let y = index as i32 / SIZE;
                        min_x = min_x.min(x);
                        max_x = max_x.max(x);
                        min_y = min_y.min(y);
                        max_y = max_y.max(y);
                    }
                    let center_is_solid = max_x >= min_x
                        && max_y >= min_y
                        && pixels[(((min_y + max_y) / 2) * SIZE + (min_x + max_x) / 2) as usize]
                            & 0xff
                            == 255;
                    (
                        (max_x - min_x + 1, max_y - min_y + 1),
                        solid,
                        smoothed,
                        center_is_solid,
                    )
                });
                let action_bounds = actions.map(|(bounds, _, _, _)| bounds);
                let action_heights = action_bounds.map(|(_, height)| height);
                let action_widths = action_bounds.map(|(width, _)| width);
                let min_width = *action_widths.iter().min().unwrap();
                let max_width = *action_widths.iter().max().unwrap();
                let min_height = *action_heights.iter().min().unwrap();
                let max_height = *action_heights.iter().max().unwrap();
                println!("{dpi} DPI (Open, Shield, Folder): {action_bounds:?}");
                PatBlt(dc, 0, 0, SIZE, SIZE, BLACKNESS);
                let font = overlay_font(dpi, 17, FW_SEMIBOLD as i32);
                assert!(!font.is_null());
                let old_font = SelectObject(dc, font);
                SetTextColor(dc, 0x00ff_ffff);
                SetBkMode(dc, TRANSPARENT as i32);
                let mut text_bounds = bounds;
                DrawTextW(
                    dc,
                    w!("Текст"),
                    5,
                    &mut text_bounds,
                    DT_SINGLELINE | DT_NOPREFIX,
                );
                GdiFlush();
                let pixels = std::slice::from_raw_parts(data.cast::<u32>(), (SIZE * SIZE) as usize);
                let mut gray_edge = false;
                let gray_text = pixels.iter().all(|&pixel| {
                    let [blue, green, red, _] = pixel.to_le_bytes();
                    gray_edge |= (1..255).contains(&red);
                    red == green && green == blue
                });
                SelectObject(dc, old_font);
                DeleteObject(font);
                PatBlt(dc, 0, 0, SIZE, SIZE, BLACKNESS);
                let label = "Калькулятор Windows".encode_utf16().collect::<Vec<_>>();
                let narrow = RECT {
                    right: 60,
                    ..bounds
                };
                let painted = renderer
                    .canvas(dc)
                    .is_some_and(|canvas| canvas.text(narrow, &label, 17.0 * scale, 0x00ff_ffff));
                GdiFlush();
                let pixels = std::slice::from_raw_parts(data.cast::<u32>(), (SIZE * SIZE) as usize);
                let mut coverage = [false; 256];
                let mut clipped = true;
                let graded_gray = pixels.iter().enumerate().all(|(index, &pixel)| {
                    let [b, g, r, _] = pixel.to_le_bytes();
                    coverage[r as usize] = true;
                    let x = index as i32 % SIZE;
                    clipped &= r == 0 || (narrow.left..narrow.right).contains(&x);
                    r == g && g == b
                });
                let coverage_levels = coverage[1..255].iter().filter(|&&gray| gray).count();
                SelectObject(dc, previous);
                DeleteObject(dib);
                DeleteDC(dc);
                assert!(shape, "rounded fill at {dpi} DPI");
                assert!(outline_corners, "rounded outline corners at {dpi} DPI");
                assert_eq!(edge_coverage[0], 255, "aligned outline at {dpi} DPI");
                assert_eq!(edge_coverage[2], 0, "outline width at {dpi} DPI");
                let coverage = edge_coverage.into_iter().sum::<u32>();
                assert!(
                    (coverage as f32 - 255.0 * scale).abs() <= 1.0,
                    "one DIP of outline coverage at {dpi} DPI: {coverage}"
                );
                assert!(
                    glyphs.into_iter().all(|has_antialias| has_antialias),
                    "search, copy, calculator and fallback symbols at {dpi} DPI: {glyphs:?}"
                );
                assert!(
                    min_height >= (17.0 * scale).floor() as i32
                        && max_height <= (18.0 * scale).ceil() as i32
                        && max_height - min_height <= 1,
                    "matched action height at {dpi} DPI: {action_bounds:?}"
                );
                assert!(
                    min_width >= (17.0 * scale).floor() as i32
                        && max_width <= (19.0 * scale).ceil() as i32
                        && max_width - min_width <= scale.ceil() as i32,
                    "matched action width at {dpi} DPI: {action_bounds:?}"
                );
                for (name, ((width, height), solid, smoothed, center)) in
                    ["Open", "Shield", "Folder"].into_iter().zip(actions)
                {
                    assert!(smoothed, "{name} has no antialiased edge at {dpi} DPI");
                    assert!(
                        center && solid * 100 >= width * height * 40,
                        "{name} looks outlined at {dpi} DPI: {solid}/{area} solid pixels, solid center={center}",
                        area = width * height,
                    );
                }
                assert!(
                    gray_edge && gray_text,
                    "grayscale text smoothing at {dpi} DPI"
                );
                assert!(
                    painted && graded_gray && coverage_levels >= 8,
                    "unhinted text at {dpi} DPI"
                );
                assert!(clipped, "long text stays inside the layout at {dpi} DPI");
            }
        }
    }
}
