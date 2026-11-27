use std::{cell::OnceCell, ptr::null_mut, rc::Rc};

use windows_sys::{
    Win32::{
        Foundation::{COLORREF, HWND, RECT},
        Graphics::{Dwm::*, Gdi::*},
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        UI::{
            Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
            Controls::{DRAWITEMSTRUCT, EM_GETCUEBANNER, EM_GETMARGINS, ODS_FOCUS, ODS_SELECTED},
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{GetFocus, SetFocus},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

use super::{
    Results, State, VISIBLE_ROWS,
    buttons::{Kind, id_action},
    ensure_selection_visible,
    raster::{Renderer, Symbol},
    wide,
};

const WIDTH: i32 = 568;
const INSET: i32 = 7;
const INPUT: i32 = 52;
const ROW: i32 = 60;
const GAP: i32 = 9;
const FRAME_RADIUS: i32 = 22;
const INPUT_RADIUS: f32 = 16.0;
const ROW_RADIUS: f32 = 14.0;
pub(super) const APP_ACTION_SIZE: i32 = 36;
pub(super) const APP_ACTION_SYMBOL: f32 = 25.0;
const COPY_SIZE: i32 = 26;
pub(super) const APP_ICON_DIP: i32 = 34;
const ROW_GAP: i32 = 4;
const ROW_ICON_LEFT: i32 = 16;
const ROW_TEXT_LEFT: i32 = 68;
const ACTION_RIGHT_INSET: i32 = 17;
const CONTENT_ACTION_GAP: i32 = 10;
const TITLE_FONT_DIP: i32 = 17;
const DETAIL_FONT_DIP: i32 = 13;
const ANSWER_FONT_DIP: i32 = 29;

#[derive(Clone, Copy)]
pub(super) struct Geometry {
    pub(super) area: RECT,
    pub(super) dpi: u32,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            area: RECT::default(),
            dpi: 96,
        }
    }
}

impl Geometry {
    pub(super) fn px(self, value: i32) -> i32 {
        (value * self.dpi as i32 + 48) / 96
    }

    fn scale(self, value: f32) -> f32 {
        value * self.dpi as f32 / 96.0
    }
}

#[derive(PartialEq, Eq)]
struct Colors {
    background: COLORREF,
    border: COLORREF,
    text: COLORREF,
    secondary: COLORREF,
    input: COLORREF,
    selection: COLORREF,
    selection_text: COLORREF,
    accent: COLORREF,
    dark: bool,
    high_contrast: bool,
    translucent: bool,
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    u32::from_le_bytes([r, g, b, 0])
}

impl Colors {
    fn system() -> Self {
        unsafe {
            let mut contrast = HIGHCONTRASTW {
                cbSize: size_of::<HIGHCONTRASTW>() as u32,
                ..Default::default()
            };
            SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                contrast.cbSize,
                (&raw mut contrast).cast(),
                0,
            );
            if contrast.dwFlags & HCF_HIGHCONTRASTON != 0 {
                return Self {
                    background: GetSysColor(COLOR_WINDOW),
                    border: GetSysColor(COLOR_WINDOWTEXT),
                    text: GetSysColor(COLOR_WINDOWTEXT),
                    secondary: GetSysColor(COLOR_WINDOWTEXT),
                    input: GetSysColor(COLOR_WINDOW),
                    selection: GetSysColor(COLOR_HIGHLIGHT),
                    selection_text: GetSysColor(COLOR_HIGHLIGHTTEXT),
                    accent: GetSysColor(COLOR_WINDOWTEXT),
                    dark: false,
                    high_contrast: true,
                    translucent: false,
                };
            }
            let setting = |name, fallback| {
                let mut value: u32 = fallback;
                let mut bytes = size_of::<u32>() as u32;
                if RegGetValueW(
                    HKEY_CURRENT_USER,
                    w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                    name,
                    RRF_RT_REG_DWORD,
                    null_mut(),
                    (&raw mut value).cast(),
                    &mut bytes,
                ) == 0
                {
                    value
                } else {
                    fallback
                }
            };
            let translucent = setting(w!("EnableTransparency"), 1) != 0;
            if setting(w!("AppsUseLightTheme"), 1) == 0 {
                Self {
                    background: rgb(34, 35, 40),
                    border: rgb(68, 71, 79),
                    text: rgb(249, 249, 250),
                    secondary: rgb(170, 175, 187),
                    input: rgb(42, 44, 50),
                    selection: rgb(48, 53, 64),
                    selection_text: rgb(246, 247, 249),
                    accent: rgb(146, 187, 253),
                    dark: true,
                    high_contrast: false,
                    translucent,
                }
            } else {
                Self {
                    background: rgb(248, 248, 250),
                    border: rgb(213, 215, 222),
                    text: rgb(29, 31, 35),
                    secondary: rgb(99, 104, 115),
                    input: rgb(255, 255, 255),
                    selection: rgb(232, 238, 249),
                    selection_text: rgb(29, 31, 35),
                    accent: rgb(42, 94, 172),
                    dark: false,
                    high_contrast: false,
                    translucent,
                }
            }
        }
    }
}

pub(super) struct Appearance {
    colors: Colors,
    dpi: u32,
    renderer: OnceCell<Rc<Renderer>>,
    input_brush: HBRUSH,
    background_brush: HBRUSH,
    query: HFONT,
    answer: HFONT,
    title: HFONT,
    detail: HFONT,
}

impl Appearance {
    pub(super) fn new(dpi: u32) -> Self {
        Self::themed(dpi, Colors::system(), OnceCell::new())
    }

    fn themed(dpi: u32, colors: Colors, renderer: OnceCell<Rc<Renderer>>) -> Self {
        let font = |dip, weight| overlay_font(dpi, dip, weight);
        Self {
            input_brush: unsafe { CreateSolidBrush(colors.input) },
            background_brush: unsafe { CreateSolidBrush(colors.background) },
            colors,
            dpi,
            renderer,
            query: font(20, FW_NORMAL as i32),
            answer: font(ANSWER_FONT_DIP, FW_NORMAL as i32),
            title: font(TITLE_FONT_DIP, FW_NORMAL as i32),
            detail: font(DETAIL_FONT_DIP, FW_NORMAL as i32),
        }
    }

    fn renderer(&self) -> &Renderer {
        self.renderer.get_or_init(Rc::default)
    }

    fn text(&self, dc: HDC, area: RECT, value: &str, role: TextRole, color: COLORREF) {
        if area.right <= area.left || area.bottom <= area.top {
            return;
        }
        let (dip, font) = match role {
            TextRole::Title => (TITLE_FONT_DIP, self.title),
            TextRole::Answer => (ANSWER_FONT_DIP, self.answer),
            TextRole::Detail => (DETAIL_FONT_DIP, self.detail),
        };
        let value = wide(value);
        let size = dip as f32 * self.dpi as f32 / 96.0;
        let done = self
            .renderer()
            .canvas(dc)
            .is_some_and(|canvas| canvas.text(area, &value[..value.len() - 1], size, color));
        if !done {
            text_gdi(dc, area, &value, font, color);
        }
    }
}

pub(super) fn release_renderer(state: &State) {
    let cached = std::mem::take(&mut state.appearance.borrow_mut().renderer);
    drop(cached);
}

#[derive(Clone, Copy)]
enum TextRole {
    Title,
    Answer,
    Detail,
}

pub(super) fn overlay_font(dpi: u32, dip: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -((dip * dpi as i32 + 48) / 96),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET.into(),
            OUT_DEFAULT_PRECIS.into(),
            CLIP_DEFAULT_PRECIS.into(),
            // Alpha blending a ClearType glyph over the desktop leaves colored
            // subpixel edges. Grayscale coverage survives that composition.
            ANTIALIASED_QUALITY.into(),
            DEFAULT_PITCH.into(),
            w!("Segoe UI"),
        )
    }
}

impl Drop for Appearance {
    fn drop(&mut self) {
        unsafe {
            for object in [
                self.input_brush,
                self.background_brush,
                self.query,
                self.answer,
                self.title,
                self.detail,
            ] {
                if !object.is_null() {
                    DeleteObject(object);
                }
            }
        }
    }
}

pub(super) fn update_appearance(hwnd: HWND, state: &State) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        let dpi = if dpi == 0 { 96 } else { dpi };
        state.geometry.set(Geometry {
            dpi,
            ..state.geometry.get()
        });
        let colors = Colors::system();
        let dark = i32::from(colors.dark);
        let corner = DWMWCP_DONOTROUND;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&raw const dark).cast(),
            size_of_val(&dark) as u32,
        );
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let layered = WS_EX_LAYERED as isize;
        if colors.translucent && style & layered == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | layered);
            if SetLayeredWindowAttributes(hwnd, 0, 247, LWA_ALPHA) == 0 {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
            }
        } else if !colors.translucent && style & layered != 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !layered);
        }
        // A single native region defines both clipping and the hit shape.
        // Suppress DWM's preset radius and border so the client can match it.
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&raw const corner).cast(),
            size_of_val(&corner) as u32,
        );
        let border = DWMWA_COLOR_NONE;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            (&raw const border).cast(),
            size_of_val(&border) as u32,
        );
        let matches = {
            let current = state.appearance.borrow();
            current.dpi == dpi && current.colors == colors
        };
        if matches {
            let font = state.appearance.borrow().query;
            SendMessageW(state.edit.get(), WM_SETFONT, font as usize, 1);
        } else {
            let renderer = state.appearance.borrow().renderer.clone();
            let appearance = Appearance::themed(dpi, colors, renderer);
            SendMessageW(state.edit.get(), WM_SETFONT, appearance.query as usize, 1);
            drop(state.appearance.replace(appearance));
        }
        SendMessageW(
            state.list.get(),
            LB_SETITEMHEIGHT,
            0,
            state.geometry.get().px(ROW) as isize,
        );
        InvalidateRect(state.edit.get(), null_mut(), 1);
        InvalidateRect(state.list.get(), null_mut(), 1);
    }
}

pub(super) fn layout(hwnd: HWND, state: &State) {
    let g = state.geometry.get();
    if g.area.right <= g.area.left || g.area.bottom <= g.area.top {
        return;
    }
    let p = |n| g.px(n);
    let rows = state.count().min(VISIBLE_ROWS) as i32;
    let show_list = rows > 0;
    let width = p(WIDTH).min((g.area.right - g.area.left - p(16)).max(1));
    let list_top = p(INSET + INPUT + GAP);
    let content = if show_list {
        list_top + rows * p(ROW)
    } else {
        p(INSET + INPUT)
    };
    let height = content + p(INSET);
    let x = g.area.left + (g.area.right - g.area.left - width) / 2;
    // Keep the caret at a stable position as the query gains or loses results.
    let base_y = g.area.top + (g.area.bottom - g.area.top) * 42 / 100 - p(INSET) - p(INPUT) / 2;
    let y = base_y.min(g.area.bottom - height - p(8)).max(g.area.top);
    unsafe {
        let mut metrics = TEXTMETRICW::default();
        let edit = state.edit.get();
        let dc = GetDC(edit);
        let query = state.appearance.borrow().query;
        let old = SelectObject(dc, query);
        GetTextMetricsW(dc, &mut metrics);
        if !old.is_null() {
            SelectObject(dc, old);
        }
        ReleaseDC(edit, dc);
        let line_height = metrics.tmHeight.max(p(20));
        let edit_y = (p(INSET) + p(INSET + INPUT) - line_height) / 2;
        SetWindowPos(
            hwnd,
            null_mut(),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_NOZORDER,
        );
        let diameter = if state.appearance.borrow().colors.high_contrast {
            0
        } else {
            p(FRAME_RADIUS * 2).min(width).min(height)
        };
        let shape = (width, height, diameter);
        if state.shape.get() != shape {
            let applied = if diameter == 0 {
                state.shape.get().2 == 0 || SetWindowRgn(hwnd, null_mut(), 1) != 0
            } else {
                let region = window_region(width, height, diameter);
                if !region.is_null() && SetWindowRgn(hwnd, region, 1) != 0 {
                    true
                } else {
                    if !region.is_null() {
                        DeleteObject(region);
                    }
                    false
                }
            };
            if applied {
                state.shape.set(shape);
            }
        }
        SetWindowPos(
            edit,
            null_mut(),
            p(INSET + ROW_TEXT_LEFT),
            edit_y,
            (width - p(INSET * 2 + ROW_TEXT_LEFT + 3)).max(0),
            line_height,
            SWP_NOACTIVATE | SWP_NOZORDER,
        );
        SetWindowPos(
            state.list.get(),
            null_mut(),
            p(INSET),
            list_top,
            list_width(width, g, state.count()),
            rows * p(ROW),
            SWP_NOACTIVATE
                | SWP_NOZORDER
                | if show_list {
                    SWP_SHOWWINDOW
                } else {
                    SWP_HIDEWINDOW
                },
        );
        if show_list {
            ensure_selection_visible(state);
        }
        layout_buttons(hwnd, state);
        if !show_list && GetFocus() == state.list.get() {
            SetFocus(edit);
        }
        InvalidateRect(hwnd, null_mut(), 0);
    }
}

unsafe fn window_region(width: i32, height: i32, diameter: i32) -> HRGN {
    // RoundRect's exclusive right/bottom coordinates must include the last
    // client pixel on the straight edges.
    unsafe { CreateRoundRectRgn(0, 0, width + 1, height + 1, diameter, diameter) }
}

fn list_width(window_width: i32, g: Geometry, count: usize) -> i32 {
    let rail = if count > VISIBLE_ROWS { g.px(8) } else { 0 };
    (window_width - g.px(INSET) * 2 - rail).max(0)
}

fn app_action_left(left: i32, right: i32, g: Geometry, column: i32) -> i32 {
    let (inset, pitch) = if right - left < g.px(260) {
        (9, 38)
    } else {
        (ACTION_RIGHT_INSET, 40)
    };
    right - g.px(inset + APP_ACTION_SIZE + (2 - column) * pitch)
}

fn copy_left(right: i32, g: Geometry) -> i32 {
    right - g.px(ACTION_RIGHT_INSET + COPY_SIZE)
}

pub(super) fn top_index(state: &State) -> usize {
    unsafe { SendMessageW(state.list.get(), LB_GETTOPINDEX, 0, 0).max(0) as usize }
}

pub(super) fn layout_buttons(hwnd: HWND, state: &State) {
    let g = state.geometry.get();
    let p = |n| g.px(n);
    let count = state.count();
    let top = top_index(state);
    state.request_icons(top);
    if state.view_top.replace(top) != top {
        state.renew_rows();
    }
    let calc = state.is_calculation();
    unsafe {
        let mut area = RECT::default();
        GetClientRect(hwnd, &mut area);
        let list_left = p(INSET);
        let list_right = list_left + list_width(area.right, g, count);
        for (id, button) in state.buttons.iter() {
            let (slot, kind) = id_action(id);
            let should_show = top + slot < count && (kind == Kind::Copy) == calc;
            let flags = GetWindowLongPtrW(button, GWL_STYLE) as u32;
            if !should_show {
                if flags & WS_VISIBLE != 0 {
                    if GetFocus() == button {
                        SetFocus(state.edit.get());
                    }
                    ShowWindow(button, SW_HIDE);
                }
                continue;
            }
            let column = match kind {
                Kind::Open => 0,
                Kind::Elevated => 1,
                Kind::Folder | Kind::Copy => 2,
            };
            let (left, size) = if kind == Kind::Copy {
                (copy_left(list_right, g), p(COPY_SIZE))
            } else {
                (
                    app_action_left(list_left, list_right, g, column),
                    p(APP_ACTION_SIZE),
                )
            };
            let y = (p(ROW) - p(ROW_GAP) - size) / 2;
            SetWindowPos(
                button,
                HWND_TOP,
                left,
                p(INSET + INPUT + GAP) + slot as i32 * p(ROW) + y,
                size,
                size,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            InvalidateRect(button, null_mut(), 0);
        }
        InvalidateRect(hwnd, null_mut(), 0);
    }
}

pub(super) fn edit_colors(dc: HDC, state: &State) -> HBRUSH {
    let a = state.appearance.borrow();
    unsafe {
        SetTextColor(dc, a.colors.text);
        SetBkColor(dc, a.colors.input);
    }
    a.input_brush
}

pub(super) fn recolor_edit_cue(state: &State) -> bool {
    let appearance = state.appearance.borrow();
    appearance.colors.dark && !appearance.colors.high_contrast
}

pub(super) fn draw_dark_edit_cue(edit: HWND, dc: HDC, state: &State) {
    unsafe {
        let mut cue = [0u16; 128];
        if SendMessageW(
            edit,
            EM_GETCUEBANNER,
            cue.as_mut_ptr() as usize,
            cue.len() as isize,
        ) == 0
        {
            return;
        }
        let Some(end) = cue.iter().position(|&unit| unit == 0) else {
            return;
        };
        if end == 0 {
            return;
        }
        let mut client = RECT::default();
        if GetClientRect(edit, &mut client) == 0 {
            return;
        }
        let margins = SendMessageW(edit, EM_GETMARGINS, 0, 0) as u32;
        let text = RECT {
            left: client.left + i32::from(margins as u16),
            right: client.right - i32::from((margins >> 16) as u16),
            ..client
        };
        let appearance = state.appearance.borrow();
        let saved = SaveDC(dc);
        if saved != 0 {
            // Keep the real EDIT banner as the source of the text. Replace
            // only the pixels where the native cue uses the dim system ink.
            fill(dc, client, appearance.colors.input);
            text_gdi(
                dc,
                text,
                &cue[..=end],
                appearance.query,
                appearance.colors.secondary,
            );
            RestoreDC(dc, saved);
        }
    }
}

pub(super) fn list_brush(state: &State) -> HBRUSH {
    state.appearance.borrow().background_brush
}

pub(super) fn hits_input(state: &State, point: isize) -> bool {
    let y = i32::from((point >> 16) as i16);
    let g = state.geometry.get();
    (g.px(INSET)..g.px(INSET) + g.px(INPUT)).contains(&y)
}

fn fill(dc: HDC, r: RECT, color: COLORREF) {
    unsafe {
        SetDCBrushColor(dc, color);
        FillRect(dc, &r, GetStockObject(DC_BRUSH));
    }
}

fn frame(dc: HDC, r: RECT, color: COLORREF) {
    unsafe {
        SetDCBrushColor(dc, color);
        FrameRect(dc, &r, GetStockObject(DC_BRUSH));
    }
}

fn text_gdi(dc: HDC, mut area: RECT, value: &[u16], font: HFONT, color: COLORREF) {
    unsafe {
        let old = SelectObject(dc, font);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color);
        DrawTextW(
            dc,
            value.as_ptr(),
            (value.len() - 1) as i32,
            &mut area,
            DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS | DT_LEFT,
        );
        if !old.is_null() {
            SelectObject(dc, old);
        }
    }
}

pub(super) fn paint(hwnd: HWND, state: &State) {
    unsafe {
        let count = state.count();
        let top = top_index(state);
        let focused = GetFocus() == state.edit.get();
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect);
        let a = state.appearance.borrow();
        let g = state.geometry.get();
        let p = |n| g.px(n);
        let colors = &a.colors;
        fill(dc, rect, colors.background);
        if colors.high_contrast {
            frame(dc, rect, colors.border);
        }
        let input = RECT {
            left: p(INSET),
            right: rect.right - p(INSET),
            top: p(INSET),
            bottom: p(INSET + INPUT),
        };
        let edge = if colors.high_contrast {
            colors.border
        } else if focused {
            blend(colors.border, colors.accent, 30)
        } else {
            blend(colors.input, colors.border, 62)
        };
        if let Some(canvas) = a.renderer().canvas(dc) {
            let radius = |dp: f32| {
                if colors.high_contrast {
                    0.0
                } else {
                    g.scale(dp)
                }
            };
            if !colors.high_contrast {
                canvas.rounded(
                    rect,
                    radius(FRAME_RADIUS as f32),
                    colors.background,
                    Some(blend(colors.background, colors.border, 65)),
                );
            }
            canvas.rounded(input, radius(INPUT_RADIUS), colors.input, Some(edge));
            let search = RECT {
                left: p(INSET + ROW_ICON_LEFT),
                right: p(INSET + ROW_ICON_LEFT + APP_ICON_DIP),
                ..input
            };
            canvas.symbol(Symbol::Search, search, g.scale(23.0), colors.secondary);
            if count > VISIBLE_ROWS {
                let track_top = p(INSET + INPUT + GAP) + p(7);
                let track = p(ROW) * VISIBLE_ROWS as i32 - p(15);
                let thumb = (track * VISIBLE_ROWS as i32 / count as i32).max(p(21));
                let offset = (track - thumb) * top.min(count - VISIBLE_ROWS) as i32
                    / (count - VISIBLE_ROWS) as i32;
                let x = rect.right - p(INSET) - p(4);
                let bar = RECT {
                    left: x,
                    right: x + p(3),
                    top: track_top + offset,
                    bottom: track_top + offset + thumb,
                };
                let ink = if colors.high_contrast {
                    colors.accent
                } else {
                    blend(colors.background, colors.text, 34)
                };
                canvas.rounded(bar, radius(1.5), ink, None);
            }
        } else {
            fill(dc, input, colors.input);
            frame(dc, input, edge);
        }
        EndPaint(hwnd, &ps);
    }
}

fn blend(base: COLORREF, tint: COLORREF, percent: u32) -> COLORREF {
    let channel = |shift: u32| {
        let a = (base >> shift) & 255;
        let b = (tint >> shift) & 255;
        ((a * (100 - percent) + b * percent + 50) / 100) << shift
    };
    channel(0) | channel(8) | channel(16)
}

fn row_fill(colors: &Colors, selected: bool) -> COLORREF {
    if selected {
        colors.selection
    } else {
        colors.background
    }
}

fn row_ink(colors: &Colors, selected: bool) -> (COLORREF, COLORREF) {
    if selected && colors.high_contrast {
        (colors.selection_text, colors.selection_text)
    } else {
        (colors.text, colors.secondary)
    }
}

fn contextual_detail<'a>(name: &str, detail: &'a str) -> Option<&'a str> {
    let detail = detail.trim();
    (!matches!(detail, "" | "Меню «Пуск»" | "Приложение Windows")
        && !detail.eq_ignore_ascii_case(name.trim()))
    .then_some(detail)
}

fn two_line_areas(area: RECT, g: Geometry) -> (RECT, RECT) {
    let split = area.top + g.px(31);
    let padding = g.px(4);
    (
        RECT {
            top: area.top + padding,
            bottom: split,
            ..area
        },
        RECT {
            top: split,
            bottom: area.bottom - padding,
            ..area
        },
    )
}

pub(super) fn draw_row(state: &State, item: &DRAWITEMSTRUCT) {
    let a = state.appearance.borrow();
    let g = state.geometry.get();
    let p = |n| g.px(n);
    let r = item.rcItem;
    let dc = item.hDC;
    let selected = item.itemState & ODS_SELECTED != 0;
    let focused = item.itemState & ODS_FOCUS != 0;
    let colors = &a.colors;
    let face = row_fill(colors, selected);
    let card = RECT {
        bottom: r.bottom - p(ROW_GAP),
        ..r
    };
    let row = item.itemID as usize;
    let (title_ink, detail_ink) = row_ink(colors, selected);
    let app_index = match &*state.results.borrow() {
        Results::Apps(indices) => indices.get(row).copied(),
        _ => None,
    };
    let calculation = state.is_calculation();
    let icon = app_index
        .map(|index| state.app_icon(index))
        .unwrap_or(null_mut());
    let size = p(APP_ICON_DIP);
    let icon_x = card.left + p(ROW_ICON_LEFT);
    let icon_y = (card.top + card.bottom - size) / 2;
    let icon_frame = RECT {
        left: icon_x,
        right: icon_x + size,
        top: icon_y,
        bottom: icon_y + size,
    };

    fill(dc, r, colors.background);
    if selected || focused || icon.is_null() {
        if let Some(canvas) = a.renderer().canvas(dc) {
            let radius = if colors.high_contrast {
                0.0
            } else {
                g.scale(ROW_RADIUS)
            };
            if selected || focused {
                let focus_ink = if colors.high_contrast && selected {
                    title_ink
                } else {
                    colors.accent
                };
                canvas.rounded(card, radius, face, focused.then_some(focus_ink));
            }
            if calculation {
                if !colors.high_contrast {
                    canvas.rounded(
                        icon_frame,
                        g.scale(9.0),
                        blend(face, colors.accent, 15),
                        None,
                    );
                }
                let ink = if colors.high_contrast {
                    title_ink
                } else {
                    colors.accent
                };
                canvas.symbol(Symbol::Formula, icon_frame, g.scale(23.0), ink);
            } else if icon.is_null() && app_index.is_some() {
                canvas.symbol(Symbol::Application, icon_frame, g.scale(28.0), detail_ink);
            }
        } else if selected || focused {
            fill(dc, card, face);
            if focused {
                frame(dc, card, title_ink);
            }
        }
    }
    if !icon.is_null() {
        unsafe {
            DrawIconEx(
                dc,
                icon_x,
                icon_y,
                icon,
                size,
                size,
                0,
                null_mut(),
                DI_NORMAL,
            );
        }
    }

    match &*state.results.borrow() {
        Results::Apps(_) => {
            let apps = state.apps.borrow();
            let Some(app) = app_index.and_then(|index| apps.get(index)) else {
                return;
            };
            let name_area = RECT {
                left: r.left + p(ROW_TEXT_LEFT),
                right: app_action_left(r.left, r.right, g, 0) - p(CONTENT_ACTION_GAP),
                ..card
            };
            if let Some(detail) = contextual_detail(&app.name, &app.detail) {
                let (title, detail_area) = two_line_areas(name_area, g);
                a.text(dc, title, &app.name, TextRole::Title, title_ink);
                a.text(dc, detail_area, detail, TextRole::Detail, detail_ink);
            } else {
                a.text(dc, name_area, &app.name, TextRole::Title, title_ink);
            }
        }
        Results::Calculation(value) => {
            let content = RECT {
                left: r.left + p(ROW_TEXT_LEFT),
                right: copy_left(r.right, g) - p(CONTENT_ACTION_GAP),
                ..card
            };
            a.text(dc, content, value, TextRole::Answer, title_ink);
        }
        _ => (),
    }
}

pub(super) fn draw_button(state: &State, item: &DRAWITEMSTRUCT) {
    let (slot, kind) = id_action(item.CtlID as usize);
    let top = top_index(state);
    let selected =
        unsafe { SendMessageW(state.list.get(), LB_GETCURSEL, 0, 0) as usize == top + slot };
    let hot = state.buttons.hot.get() == item.hwndItem;
    let pressed = item.itemState & ODS_SELECTED != 0;
    let focused = item.itemState & ODS_FOCUS != 0;
    let a = state.appearance.borrow();
    let g = state.geometry.get();
    let colors = &a.colors;
    let r = item.rcItem;
    let dc = item.hDC;
    let background = row_fill(colors, selected);
    let (ink, quiet) = row_ink(colors, selected);
    fill(dc, r, background);
    let symbol = match kind {
        Kind::Open => Symbol::Open,
        Kind::Elevated => Symbol::Shield,
        Kind::Folder => Symbol::Folder,
        Kind::Copy => Symbol::Copy,
    };
    let active = pressed || hot || focused;
    let mark = if active {
        ink
    } else if selected && !colors.high_contrast {
        blend(quiet, ink, 35)
    } else {
        quiet
    };
    if let Some(canvas) = a.renderer().canvas(dc) {
        if active {
            let face = if colors.high_contrast {
                background
            } else {
                blend(background, ink, if pressed { 18 } else { 9 })
            };
            let radius = if colors.high_contrast {
                0.0
            } else {
                g.scale(9.0)
            };
            let edge = (focused || colors.high_contrast).then_some(mark);
            canvas.rounded(r, radius, face, edge);
        }
        let size = if kind == Kind::Copy {
            21.0
        } else {
            APP_ACTION_SYMBOL
        };
        canvas.symbol(symbol, r, g.scale(size), mark);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_region_cuts_only_the_corner_points_at_common_dpis() {
        for dpi in [96, 120, 144, 192] {
            let g = Geometry {
                dpi,
                ..Geometry::default()
            };
            let width = g.px(WIDTH);
            let height = g.px(INSET + INPUT) + g.px(INSET);
            let inset = g.px(INSET);
            unsafe {
                let region = window_region(width, height, g.px(FRAME_RADIUS * 2));
                assert!(!region.is_null());
                let contains = |x, y| PtInRegion(region, x, y) != 0;
                let clipped = !contains(0, 0)
                    && !contains(width - 1, 0)
                    && !contains(0, height - 1)
                    && !contains(width - 1, height - 1);
                let intact = contains(width / 2, 0)
                    && contains(width / 2, height - 1)
                    && contains(0, height / 2)
                    && contains(width - 1, height / 2)
                    && contains(inset, inset);
                DeleteObject(region);
                assert!(clipped && intact, "Unexpected frame clipping at {dpi} DPI");
            }
        }
    }

    #[test]
    fn trailing_actions_keep_names_and_button_hitboxes_separate_at_common_dpis() {
        for dpi in [96, 120, 144, 192] {
            let geometry = Geometry {
                dpi,
                ..Geometry::default()
            };
            let p = |value| geometry.px(value);
            let card = RECT {
                bottom: p(ROW) - p(ROW_GAP),
                ..Default::default()
            };
            let (title, detail) = two_line_areas(card, geometry);
            assert_eq!(title.bottom, detail.top);
            assert!(title.bottom - title.top >= p(TITLE_FONT_DIP + 5));
            assert!(detail.bottom - detail.top >= p(DETAIL_FONT_DIP + 5));
            assert!(p(APP_ACTION_SIZE) < card.bottom);
            for width in [240, WIDTH] {
                let r = list_width(p(width), geometry, VISIBLE_ROWS + 1);
                let open = app_action_left(0, r, geometry, 0);
                let elevate = app_action_left(0, r, geometry, 1);
                let folder = app_action_left(0, r, geometry, 2);
                assert!(p(ROW_ICON_LEFT + APP_ICON_DIP) < p(ROW_TEXT_LEFT));
                assert!(p(ROW_TEXT_LEFT) < open - p(CONTENT_ACTION_GAP));
                assert!(open + p(APP_ACTION_SIZE) < elevate);
                assert!(elevate + p(APP_ACTION_SIZE) < folder);
                assert!(folder + p(APP_ACTION_SIZE) < r);
                assert_eq!(copy_left(r, geometry), r - p(43));
            }
        }
    }

    #[test]
    fn result_detail_only_keeps_context_that_distinguishes_the_application() {
        assert_eq!(contextual_detail("MPC-BE x64", " mpc-be x64 "), None);
        assert_eq!(contextual_detail("Microsoft Edge", "Меню «Пуск»"), None);
        assert_eq!(
            contextual_detail("Microsoft Store", "Приложение Windows"),
            None
        );
        assert_eq!(
            contextual_detail("Windows PowerShell 5.1", "Windows PowerShell"),
            Some("Windows PowerShell"),
        );
    }
}
