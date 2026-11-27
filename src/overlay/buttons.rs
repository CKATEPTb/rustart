use std::{cell::Cell, ptr::null_mut};

use windows_sys::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            GetStockObject, InflateRect, InvalidateRect, MapWindowPoints, NULL_BRUSH, ValidateRect,
        },
        UI::{
            Controls::*,
            Input::KeyboardAndMouse::{SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

use super::{State, VISIBLE_ROWS, control_proc};
use crate::i18n::{ControlLabel, wide};

const FIRST: usize = 110;
const COUNT: usize = VISIBLE_ROWS * 3;
pub(super) const COPY: usize = FIRST + COUNT;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Open,
    Elevated,
    Folder,
    Copy,
}

impl Kind {
    pub(super) const APP: [Self; 3] = [Self::Open, Self::Elevated, Self::Folder];

    pub(super) fn id(self, slot: usize) -> usize {
        match self {
            Self::Open => FIRST + slot * 3,
            Self::Elevated => FIRST + slot * 3 + 1,
            Self::Folder => FIRST + slot * 3 + 2,
            Self::Copy => COPY,
        }
    }

    fn name(self) -> PCWSTR {
        wide(match self {
            Self::Open => ControlLabel::ActionOpen,
            Self::Elevated => ControlLabel::ActionElevated,
            Self::Folder => ControlLabel::ActionFolder,
            Self::Copy => ControlLabel::ActionCopy,
        })
    }
}

pub(super) fn is_button(id: usize) -> bool {
    (FIRST..=COPY).contains(&id)
}

pub(super) fn id_action(id: usize) -> (usize, Kind) {
    if id == COPY {
        return (0, Kind::Copy);
    }
    let number = id - FIRST;
    (number / 3, Kind::APP[number % 3])
}

pub(super) struct Buttons {
    handles: [Cell<HWND>; COUNT + 1],
    tooltip: Cell<HWND>,
    pub(super) hot: Cell<HWND>,
}

impl Buttons {
    pub(super) fn new() -> Self {
        Self {
            handles: std::array::from_fn(|_| Cell::new(null_mut())),
            tooltip: Cell::new(null_mut()),
            hot: Cell::new(null_mut()),
        }
    }

    pub(super) fn get(&self, id: usize) -> Option<HWND> {
        if is_button(id) {
            Some(self.handles[id - FIRST].get())
        } else {
            None
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (usize, HWND)> + '_ {
        self.handles
            .iter()
            .enumerate()
            .map(|(offset, button)| (FIRST + offset, button.get()))
    }

    pub(super) fn create(&self, owner: HWND, list: HWND, module: HINSTANCE) -> bool {
        unsafe {
            // Tooltips are owned by the overlay and destroyed with it.
            let tooltip = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                TOOLTIPS_CLASSW,
                null_mut(),
                WS_POPUP | TTS_NOPREFIX,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                owner,
                null_mut(),
                module,
                null_mut(),
            );
            self.tooltip.set(tooltip);
            if !tooltip.is_null() {
                SendMessageW(tooltip, TTM_SETMAXTIPWIDTH, 0, 350);
            }
            for (offset, handle) in self.handles.iter().enumerate() {
                let id = FIRST + offset;
                let label = id_action(id).1.name();
                let button = CreateWindowExW(
                    0,
                    w!("BUTTON"),
                    label,
                    WS_CHILD | WS_TABSTOP | WS_CLIPSIBLINGS | BS_OWNERDRAW as u32,
                    0,
                    0,
                    0,
                    0,
                    list,
                    id as _,
                    module,
                    null_mut(),
                );
                if button.is_null()
                    || SetWindowSubclass(button, Some(control_proc), id, owner as usize) == 0
                {
                    return false;
                }
                handle.set(button);
                if !tooltip.is_null() {
                    let tool = tooltip_info(list, button, label);
                    SendMessageW(tooltip, TTM_ADDTOOLW, 0, (&raw const tool) as isize);
                }
            }
            true
        }
    }

    pub(super) fn refresh_language(&self, list: HWND) {
        let tooltip = self.tooltip.get();
        unsafe {
            if !tooltip.is_null() {
                SendMessageW(tooltip, TTM_POP, 0, 0);
            }
            for (id, button) in self.iter().filter(|(_, button)| !button.is_null()) {
                let label = id_action(id).1.name();
                SetWindowTextW(button, label);
                if !tooltip.is_null() {
                    let tool = tooltip_info(list, button, label);
                    SendMessageW(tooltip, TTM_UPDATETIPTEXTW, 0, (&raw const tool) as isize);
                }
            }
        }
    }

    pub(super) fn hover(&self, hwnd: HWND) {
        if self.hot.get() == hwnd {
            return;
        }
        let mut event = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        unsafe {
            if TrackMouseEvent(&mut event) != 0 {
                let previous = self.hot.replace(hwnd);
                if !previous.is_null() {
                    invalidate_action(previous);
                }
                invalidate_action(hwnd);
            }
        }
    }

    pub(super) fn leave(&self, hwnd: HWND) {
        if self.hot.get() == hwnd {
            self.hot.set(null_mut());
            unsafe {
                InvalidateRect(hwnd, null_mut(), 0);
            }
        }
    }
}

pub(super) fn paint_message(
    state: &State,
    hwnd: HWND,
    id: usize,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> Option<LRESULT> {
    unsafe {
        if hwnd == state.list.get() {
            match message {
                WM_DRAWITEM => {
                    let item = (lp as *const DRAWITEMSTRUCT).as_ref()?;
                    let button = state.buttons.get(wp)?;
                    if button == item.hwndItem {
                        invalidate_action(button);
                        return Some(1);
                    }
                }
                WM_CTLCOLORBTN if state.buttons.iter().any(|(_, button)| button == lp as HWND) => {
                    return Some(GetStockObject(NULL_BRUSH) as LRESULT);
                }
                _ => {}
            }
        }
        if !is_button(id) {
            return None;
        }
        // Native buttons supply interaction and accessibility. The list owns
        // their pixels so scrolling and selection repaint the entire row.
        match message {
            WM_PAINT => {
                ValidateRect(hwnd, null_mut());
                Some(0)
            }
            WM_ERASEBKGND => Some(1),
            WM_PRINT | WM_PRINTCLIENT => Some(0),
            WM_SETFOCUS | WM_KILLFOCUS | WM_CAPTURECHANGED | BM_SETSTATE => {
                let result = DefSubclassProc(hwnd, message, wp, lp);
                invalidate_action(hwnd);
                Some(result)
            }
            _ => None,
        }
    }
}

fn invalidate_action(hwnd: HWND) {
    unsafe {
        let list = GetParent(hwnd);
        let mut rect = RECT::default();
        if !list.is_null() && GetWindowRect(hwnd, &mut rect) != 0 {
            MapWindowPoints(null_mut(), list, (&raw mut rect).cast(), 2);
            // A region completely covered by a child does not invalidate its parent.
            InflateRect(&mut rect, 1, 1);
            InvalidateRect(list, &rect, 0);
        }
    }
}

fn tooltip_info(owner: HWND, button: HWND, label: PCWSTR) -> TTTOOLINFOW {
    TTTOOLINFOW {
        cbSize: size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_IDISHWND | TTF_SUBCLASS,
        hwnd: owner,
        uId: button as usize,
        lpszText: label.cast_mut(),
        ..Default::default()
    }
}

pub(super) fn tab(state: &State, current: HWND, reverse: bool) {
    let controls: Vec<_> = [state.edit.get(), state.list.get()]
        .into_iter()
        .chain(state.buttons.iter().map(|(_, handle)| handle))
        .filter(|&handle| unsafe { IsWindowVisible(handle) != 0 })
        .collect();
    let Some(index) = controls.iter().position(|&handle| handle == current) else {
        return;
    };
    let offset = if reverse { controls.len() - 1 } else { 1 };
    unsafe {
        SetFocus(controls[(index + offset) % controls.len()]);
    }
}
