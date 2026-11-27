mod capture;
mod paint;
mod popup;

use std::{cell::Cell, io, ptr::null_mut};

use windows_sys::{
    Win32::{
        Foundation::{HWND, POINT, RECT},
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::w,
};

use crate::{
    i18n::{Language, Msg, tr},
    prefs::Prefs,
    runtime,
};

const CALLBACK: u32 = WM_APP + 41;
const STARTUP_FINISHED: u32 = WM_APP + 46;
const RETRY: usize = 42;
const ICON: u32 = 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

thread_local! {
    static EXPLORER_RESTARTED: Cell<u32> = const { Cell::new(0) };
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn scale(dip: i32, dpi: u32) -> i32 {
    (dip * dpi as i32 + 48) / 96
}

fn icon_resource() -> *const u16 {
    // Win32 treats pointer values with a zero high word as resource IDs.
    std::ptr::without_provenance(ICON as usize)
}

fn icon_data(owner: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: owner,
        uID: ICON,
        ..Default::default()
    }
}

fn register(owner: HWND) -> bool {
    let mut data = icon_data(owner);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = CALLBACK;
    let tip = wide("Rustart");
    data.szTip[..tip.len()].copy_from_slice(&tip);
    unsafe {
        let module = GetModuleHandleW(null_mut());
        // The small icon sizes are native DIBs, so showing the tray does not
        // need the PNG codec or a scaled copy of the large Explorer icon.
        data.hIcon = LoadImageW(
            module,
            icon_resource(),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        );
        if data.hIcon.is_null() {
            data.hIcon = LoadIconW(module, icon_resource());
        }
        let registered =
            Shell_NotifyIconW(NIM_ADD, &data) != 0 || Shell_NotifyIconW(NIM_MODIFY, &data) != 0;
        if registered {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            if Shell_NotifyIconW(NIM_SETVERSION, &data) != 0 {
                KillTimer(owner, RETRY);
                return true;
            }
            Shell_NotifyIconW(NIM_DELETE, &data);
        }
        SetTimer(owner, RETRY, 2500, None) != 0
    }
}

pub fn install(owner: HWND) -> Result<(), String> {
    let restart = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if restart == 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    EXPLORER_RESTARTED.set(restart);
    // Explorer can deliver icon events to a resident elevated above it.
    for message in [restart, CALLBACK] {
        unsafe { ChangeWindowMessageFilterEx(owner, message, MSGFLT_ALLOW, null_mut()) };
    }
    // Explorer can start later during sign-in.
    if register(owner) {
        Ok(())
    } else {
        Err(crate::i18n::tr(crate::i18n::Msg::TrayIconUnavailable).into())
    }
}

pub fn remove(owner: HWND) {
    popup::close();
    unsafe {
        KillTimer(owner, RETRY);
        Shell_NotifyIconW(NIM_DELETE, &icon_data(owner));
    }
}

fn anchor(owner: HWND, wp: usize) -> POINT {
    let ids = NOTIFYICONIDENTIFIER {
        cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: owner,
        uID: ICON,
        ..Default::default()
    };
    let mut rect = RECT::default();
    unsafe {
        if Shell_NotifyIconGetRect(&ids, &mut rect) >= 0 {
            return POINT {
                x: (rect.left + rect.right) / 2,
                y: rect.top,
            };
        }
    }
    let point = POINT {
        x: wp as i16 as i32,
        y: (wp >> 16) as i16 as i32,
    };
    if point.x == -1 && point.y == -1 {
        let mut cursor = POINT::default();
        unsafe { GetCursorPos(&mut cursor) };
        cursor
    } else {
        point
    }
}

pub fn handle(owner: HWND, msg: u32, wp: usize, lp: isize, prefs: &Prefs) -> Option<isize> {
    if msg == STARTUP_FINISHED {
        popup::finish_startup_change(owner);
        return Some(0);
    }
    if (msg == WM_TIMER && wp == RETRY) || (msg != 0 && EXPLORER_RESTARTED.get() == msg) {
        register(owner);
        return Some(0);
    }
    if msg == CALLBACK && ((lp as u32) >> 16) == ICON {
        match (lp as u32) & 0xffff {
            NIN_SELECT | NIN_KEYSELECT => {
                popup::close();
                unsafe { PostMessageW(owner, runtime::SHOW, 0, 0) };
            }
            WM_CONTEXTMENU if !popup::close() => {
                popup::show(owner, anchor(owner, wp), prefs, wp as u32 == u32::MAX);
            }
            _ => {}
        }
        return Some(0);
    }
    None
}

pub fn editing_hotkey() -> bool {
    capture::active()
}

pub fn cancel_hotkey_capture() {
    popup::cancel_capture();
}

fn report(owner: HWND, message: &str) {
    unsafe {
        MessageBoxW(
            owner,
            wide(message).as_ptr(),
            w!("Rustart"),
            MB_OK | MB_ICONERROR,
        )
    };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Startup,
    Admin,
    Hotkey,
    Languages,
    Back,
    ChooseLanguage(Option<Language>),
    Quit,
}

#[derive(Clone)]
struct Row {
    action: Option<Action>,
    startup_mode: Option<crate::startup::Mode>,
    label: &'static str,
    glyph: &'static str,
    detail: String,
    checked: Option<bool>,
    separator: bool,
    enabled: bool,
    note: String,
}

impl Row {
    fn accessible_name(&self) -> String {
        if self.action == Some(Action::Back) {
            return format!("{}, {}", self.label, tr(Msg::TrayBack));
        }
        match self.checked {
            Some(checked) if matches!(self.action, Some(Action::ChooseLanguage(_))) => {
                if checked {
                    format!("{}, {}", self.label, tr(Msg::TraySelected))
                } else {
                    self.label.into()
                }
            }
            Some(true) => format!("{}, {}", self.label, tr(Msg::TrayOn)),
            Some(false) => format!("{}, {}", self.label, tr(Msg::TrayOff)),
            None if !self.note.is_empty() => format!("{}, {}", self.label, self.note),
            None if !self.detail.is_empty() => format!("{}, {}", self.label, self.detail),
            None => self.label.into(),
        }
    }

    fn new(action: Action, label: &'static str, glyph: &'static str) -> Self {
        Self {
            action: Some(action),
            startup_mode: None,
            label,
            glyph,
            detail: String::new(),
            checked: None,
            separator: false,
            enabled: true,
            note: String::new(),
        }
    }
}
