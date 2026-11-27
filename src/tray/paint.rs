mod toggle;

use std::ptr::null_mut;

use windows_sys::Win32::{
    Foundation::{COLORREF, RECT},
    Graphics::Gdi::*,
    System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    UI::{
        Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        Controls::{DRAWITEMSTRUCT, ODS_SELECTED},
        WindowsAndMessaging::*,
    },
};
use windows_sys::core::{PCWSTR, w};

use super::{Action, Row, scale};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Colors {
    pub background: COLORREF,
    pub border: COLORREF,
    pub(crate) text: COLORREF,
    pub(crate) secondary: COLORREF,
    pub(super) disabled: COLORREF,
    pub(super) hover: COLORREF,
    pub(super) hover_text: COLORREF,
    pub(super) accent: COLORREF,
    pub(super) on_accent: COLORREF,
    pub dark: bool,
    pub(crate) high_contrast: bool,
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    u32::from_le_bytes([r, g, b, 0])
}

fn personalization_enabled(name: PCWSTR) -> bool {
    let mut value = 1u32;
    let mut bytes = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            name,
            RRF_RT_REG_DWORD,
            null_mut(),
            (&raw mut value).cast(),
            &mut bytes,
        )
    };
    status != 0 || value != 0
}

impl Colors {
    pub fn current() -> Self {
        unsafe {
            let mut hc = HIGHCONTRASTW {
                cbSize: size_of::<HIGHCONTRASTW>() as u32,
                ..Default::default()
            };
            SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.cbSize, (&raw mut hc).cast(), 0);
            if hc.dwFlags & HCF_HIGHCONTRASTON != 0 {
                return Self {
                    background: GetSysColor(COLOR_MENU),
                    border: GetSysColor(COLOR_MENUTEXT),
                    text: GetSysColor(COLOR_MENUTEXT),
                    secondary: GetSysColor(COLOR_MENUTEXT),
                    disabled: GetSysColor(COLOR_GRAYTEXT),
                    hover: GetSysColor(COLOR_HIGHLIGHT),
                    hover_text: GetSysColor(COLOR_HIGHLIGHTTEXT),
                    accent: GetSysColor(COLOR_HIGHLIGHT),
                    on_accent: GetSysColor(COLOR_HIGHLIGHTTEXT),
                    dark: false,
                    high_contrast: true,
                };
            }
        }
        Self::themed(!personalization_enabled(w!("AppsUseLightTheme")))
    }

    pub(crate) fn themed(dark: bool) -> Self {
        if dark {
            Self {
                background: rgb(43, 45, 49),
                border: rgb(69, 72, 78),
                text: rgb(246, 247, 249),
                secondary: rgb(199, 201, 207),
                disabled: rgb(147, 150, 156),
                hover: rgb(60, 63, 68),
                hover_text: rgb(255, 255, 255),
                accent: rgb(108, 199, 255),
                on_accent: rgb(18, 41, 57),
                dark: true,
                high_contrast: false,
            }
        } else {
            Self {
                background: rgb(249, 249, 250),
                border: rgb(212, 214, 218),
                text: rgb(29, 31, 35),
                secondary: rgb(86, 90, 97),
                disabled: rgb(112, 115, 119),
                hover: rgb(235, 237, 240),
                hover_text: rgb(19, 22, 25),
                accent: rgb(0, 100, 172),
                on_accent: rgb(255, 255, 255),
                dark: false,
                high_contrast: false,
            }
        }
    }
}

pub(crate) struct Fonts {
    pub(crate) regular: HFONT,
    pub(crate) small: HFONT,
    pub(crate) heading: HFONT,
    pub(super) symbol: HFONT,
}

impl Fonts {
    pub fn new(dpi: u32) -> Self {
        let font = |dip, weight, name| unsafe {
            CreateFontW(
                -scale(dip, dpi),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET.into(),
                0,
                0,
                CLEARTYPE_QUALITY.into(),
                0,
                name,
            )
        };
        Self {
            regular: font(14, 400, w!("Segoe UI")),
            small: font(13, 400, w!("Segoe UI")),
            heading: font(16, 600, w!("Segoe UI")),
            symbol: font(16, 400, w!("Segoe Fluent Icons")),
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for font in [self.regular, self.small, self.heading, self.symbol] {
            if !font.is_null() {
                unsafe { DeleteObject(font) };
            }
        }
    }
}

pub(super) fn fill(dc: HDC, rect: &RECT, color: COLORREF, radius: i32) {
    unsafe {
        let brush = GetStockObject(DC_BRUSH);
        SetDCBrushColor(dc, color);
        if radius == 0 {
            FillRect(dc, rect, brush);
        } else {
            let old_brush = SelectObject(dc, brush);
            let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
            RoundRect(
                dc,
                rect.left,
                rect.top,
                rect.right + 1,
                rect.bottom + 1,
                radius * 2,
                radius * 2,
            );
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
        }
    }
}

pub(super) fn text(
    dc: HDC,
    mut rect: RECT,
    value: &str,
    font: HFONT,
    color: COLORREF,
    flags: u32,
) -> i32 {
    if value.is_empty() {
        return 0;
    }
    let wide = super::wide(value);
    unsafe {
        let old = SelectObject(dc, font);
        SetTextColor(dc, color);
        DrawTextW(
            dc,
            wide.as_ptr(),
            (wide.len() - 1) as i32,
            &mut rect,
            DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | flags,
        );
        SelectObject(dc, old);
    }
    rect.right - rect.left
}

pub(super) struct Painter<'a> {
    pub dpi: u32,
    pub colors: Colors,
    pub fonts: &'a Fonts,
    pub header_icon: HICON,
    pub keyboard: bool,
}

impl Painter<'_> {
    pub fn row(&self, item: &DRAWITEMSTRUCT, row: &Row, hover: bool, focus: bool) {
        let p = |n| scale(n, self.dpi);
        let dc = item.hDC;
        let mut rc = item.rcItem;
        let c = self.colors;
        unsafe { SetBkMode(dc, TRANSPARENT as i32) };
        fill(dc, &rc, c.background, 0);
        if row.separator {
            fill(
                dc,
                &RECT {
                    left: p(6),
                    right: rc.right - p(6),
                    top: p(4),
                    bottom: p(4) + 1,
                },
                c.border,
                0,
            );
            rc.top += p(9);
        }
        let header = row.action.is_none();
        let selected = !header
            && row.enabled
            && (hover || item.itemState & ODS_SELECTED != 0 || (focus && self.keyboard));
        let fg = if selected {
            c.hover_text
        } else if row.enabled {
            c.text
        } else {
            c.disabled
        };
        if selected {
            fill(dc, &rc, c.hover, p(4));
        }

        let mid = (rc.top + rc.bottom) / 2;
        let mut label = RECT {
            left: p(40),
            right: rc.right - p(12),
            ..rc
        };
        if header {
            let size = p(20);
            unsafe {
                DrawIconEx(
                    dc,
                    p(10),
                    mid - size / 2,
                    self.header_icon,
                    size,
                    size,
                    0,
                    null_mut(),
                    DI_NORMAL,
                )
            };
        } else {
            let glyph = if matches!(row.action, Some(Action::ChooseLanguage(_)))
                && row.checked == Some(true)
            {
                "\u{e73e}"
            } else {
                row.glyph
            };
            text(
                dc,
                RECT {
                    left: p(4),
                    right: p(36),
                    ..rc
                },
                glyph,
                self.fonts.symbol,
                fg,
                DT_CENTER,
            );
        }

        if row.action == Some(Action::Languages) {
            text(
                dc,
                RECT {
                    left: label.right - p(14),
                    ..label
                },
                "\u{e76c}",
                self.fonts.symbol,
                fg,
                DT_RIGHT,
            );
            label.right -= p(20);
        }
        let sub = if selected { c.hover_text } else { c.secondary };
        if !row.note.is_empty() {
            let split = rc.top + (rc.bottom - rc.top) / 2;
            text(
                dc,
                RECT {
                    top: split,
                    ..label
                },
                &row.note,
                self.fonts.small,
                sub,
                DT_LEFT | DT_END_ELLIPSIS,
            );
            label.bottom = split;
        }
        if let Some(checked) = row.checked
            && !matches!(row.action, Some(Action::ChooseLanguage(_)))
        {
            let left = rc.right - p(44);
            self.draw_toggle(dc, left, mid, selected, checked);
            label.right = left - p(8);
        } else if !row.detail.is_empty() {
            let width = text(dc, label, &row.detail, self.fonts.small, sub, DT_CALCRECT)
                .min((label.right - label.left).max(0) / 2);
            text(
                dc,
                RECT {
                    left: label.right - width,
                    ..label
                },
                &row.detail,
                self.fonts.small,
                sub,
                DT_RIGHT | DT_END_ELLIPSIS,
            );
            label.right -= width + p(12);
        }
        let font = if header {
            self.fonts.heading
        } else {
            self.fonts.regular
        };
        text(dc, label, row.label, font, fg, DT_LEFT | DT_END_ELLIPSIS);
        if selected && focus && self.keyboard {
            let ring = RECT {
                left: rc.left + 2,
                top: rc.top + 2,
                right: rc.right - 2,
                bottom: rc.bottom - 2,
            };
            unsafe { DrawFocusRect(dc, &ring) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_details_have_zero_width_without_calling_user32() {
        assert_eq!(
            text(null_mut(), RECT::default(), "", null_mut(), 0, DT_CALCRECT),
            0
        );
    }
}
