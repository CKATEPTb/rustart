use std::{
    cell::{Cell, RefCell},
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, TryRecvError},
};

use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{Dwm::*, Gdi::*},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::{DRAWITEMSTRUCT, LoadIconWithScaleDown, SetScrollInfo, WM_MOUSELEAVE},
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::{
                GetFocus, GetKeyState, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
                VK_BACK, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR,
                VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
            },
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

use super::{
    Action, Row, STARTUP_FINISHED,
    capture::{self, Capture, Event},
    paint::{self, Colors, Fonts, Painter},
    report, scale, wide,
};
use crate::{
    i18n::{Language, Msg, render, tr},
    prefs::Prefs,
    runtime,
    startup::{self, Mode},
};

const CLASS: *const u16 = w!("Rustart.TrayMenu");
const BUTTON: usize = 100;
const CANCEL_CAPTURE: u32 = WM_APP + 45;
const CHOICE_PAGE_SIZE: usize = 7;
thread_local! {
    static OPEN: Cell<HWND> = const { Cell::new(null_mut()) };
    static STARTUP_CHANGE: RefCell<Option<StartupChange>> = const { RefCell::new(None) };
}

struct StartupChange {
    expected: Mode,
    result: Receiver<Result<(), String>>,
}

struct Menu {
    owner: HWND,
    anchor: POINT,
    prefs: RefCell<Prefs>,
    page: Page,
    rows: Vec<Row>,
    dpi: u32,
    fonts: Fonts,
    colors: Cell<Colors>,
    icon: HICON,
    hot: Cell<HWND>,
    keyboard: Cell<bool>,
    capture: RefCell<Option<Capture>>,
    note: RefCell<String>,
    refresh_startup: Cell<bool>,
    scroll: Cell<usize>,
    wheel: Cell<i32>,
    visible_choices: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    Languages,
}

enum Transition {
    Stay,
    Startup {
        owner: HWND,
        expected: Mode,
        desired: Mode,
    },
    Page {
        owner: HWND,
        anchor: POINT,
        prefs: Prefs,
        keyboard: bool,
        page: Page,
        focus: Option<Action>,
    },
    Quit(HWND),
    Error(HWND, String),
}

impl Transition {
    fn run(self) {
        match self {
            Self::Stay => {}
            Self::Startup {
                owner,
                expected,
                desired,
            } => begin_startup_change(owner, expected, desired),
            Self::Page {
                owner,
                anchor,
                prefs,
                keyboard,
                page,
                focus,
            } => {
                // Transfer foreground focus while the old pane still owns it.
                // Destroying it first can activate Explorer and lock out
                // SetForegroundWindow for the replacement pane.
                let previous = OPEN.get();
                show_page(owner, anchor, &prefs, keyboard, page, focus);
                if !previous.is_null() && previous != OPEN.get() {
                    unsafe { DestroyWindow(previous) };
                }
            }
            Self::Quit(owner) => {
                close();
                unsafe { PostMessageW(owner, runtime::QUIT, 0, 0) };
            }
            Self::Error(owner, error) => {
                close();
                report(owner, &error);
            }
        }
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        if !self.icon.is_null() {
            unsafe { DestroyIcon(self.icon) };
        }
    }
}

fn rows(prefs: &Prefs, page: Page) -> Vec<Row> {
    if page == Page::Languages {
        let mut rows = vec![Row::new(Action::Back, tr(Msg::TrayLanguage), "\u{e72b}")];
        for choice in std::iter::once(None).chain(Language::ALL.into_iter().map(Some)) {
            let mut row = Row::new(
                Action::ChooseLanguage(choice),
                choice.map_or_else(|| tr(Msg::TraySystemLanguage), Language::native_name),
                "",
            );
            row.checked = Some(choice == prefs.language);
            rows.push(row);
        }
        return rows;
    }
    let pending = STARTUP_CHANGE.with_borrow(|pending| pending.as_ref().map(|job| job.expected));
    main_rows(prefs, pending.map_or_else(startup::read, Ok))
}

fn main_rows(prefs: &Prefs, mode: Result<Mode, String>) -> Vec<Row> {
    let mut rows = vec![Row {
        action: None,
        startup_mode: None,
        glyph: "",
        label: "Rustart",
        detail: String::new(),
        checked: None,
        separator: false,
        enabled: true,
        note: String::new(),
    }];

    let mut hotkey = Row::new(Action::Hotkey, tr(Msg::TrayHotkey), "\u{e765}");
    hotkey.detail = capture::hotkey_label(prefs.hotkey);
    rows.push(hotkey);
    let mut language = Row::new(Action::Languages, tr(Msg::TrayLanguage), "\u{e774}");
    language.detail = prefs
        .language
        .map_or_else(|| tr(Msg::TraySystemLanguage), Language::native_name)
        .into();
    language.separator = true;
    rows.push(language);

    let mut startup = Row::new(Action::Startup, tr(Msg::TrayStartup), "\u{e768}");
    startup.startup_mode = mode.as_ref().ok().copied();
    startup.enabled = mode.is_ok();
    startup.checked = mode
        .is_ok()
        .then_some(matches!(&mode, Ok(Mode::Standard | Mode::Administrator)));
    match &mode {
        Ok(Mode::DisabledByWindows) => startup.label = tr(Msg::TrayStartupDisabled),
        Err(_) => startup.detail = tr(Msg::TrayStartupUnavailable).into(),
        _ => {}
    }
    rows.push(startup);
    if let Ok(mode @ (Mode::Standard | Mode::Administrator)) = mode {
        let mut admin = Row::new(Action::Admin, tr(Msg::TrayAdmin), "\u{ea18}");
        admin.startup_mode = Some(mode);
        admin.checked = Some(mode == Mode::Administrator);
        rows.push(admin);
    }

    let mut quit = Row::new(Action::Quit, tr(Msg::TrayExit), "\u{e7e8}");
    quit.separator = true;
    rows.push(quit);
    rows
}

fn next_startup(mode: Mode, administrator: bool) -> Mode {
    match (mode, administrator) {
        (Mode::DisabledByWindows, _) => Mode::DisabledByWindows,
        (Mode::Off, false) | (Mode::Administrator, true) => Mode::Standard,
        (Mode::Standard, true) => Mode::Administrator,
        _ => Mode::Off,
    }
}

fn startup_pending() -> bool {
    STARTUP_CHANGE.with_borrow(Option::is_some)
}

fn begin_startup_change(owner: HWND, expected: Mode, desired: Mode) {
    if startup_pending() {
        return;
    }
    let (sender, receiver) = mpsc::channel();
    let window = owner as usize;
    // UAC can keep the worker occupied after the popup itself has closed.
    // The owner receives only a wake-up message. Results stay in this process.
    let worker = std::thread::Builder::new().spawn(move || {
        let result = startup::read().and_then(|mode| {
            if mode == expected {
                startup::write(desired)
            } else {
                Ok(())
            }
        });
        let _ = sender.send(result);
        unsafe { PostMessageW(window as HWND, STARTUP_FINISHED, 0, 0) };
    });
    match worker {
        Ok(_) => {
            STARTUP_CHANGE.set(Some(StartupChange {
                expected,
                result: receiver,
            }));
            let popup = OPEN.get();
            if !popup.is_null() {
                unsafe {
                    RedrawWindow(popup, null(), null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN)
                };
            }
        }
        Err(error) => report(
            owner,
            &render(Msg::StartupFailed, &[("error", &error.to_string())]),
        ),
    }
}

pub(super) fn finish_startup_change(owner: HWND) {
    let result = STARTUP_CHANGE.with_borrow_mut(|pending| {
        let result = match pending.as_ref()?.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(error @ TryRecvError::Disconnected) => {
                Err(render(Msg::StartupFailed, &[("error", &error.to_string())]))
            }
        };
        *pending = None;
        Some(result)
    });
    match result {
        Some(Ok(())) => refresh_startup_menu(),
        Some(Err(error)) => Transition::Error(owner, error).run(),
        None => {}
    }
}

fn refresh_startup_menu() {
    let hwnd = OPEN.get();
    if hwnd.is_null() || unsafe { GetForegroundWindow() } != hwnd {
        return;
    }
    let transition = unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Menu;
        let Some(menu) = ptr.as_ref().filter(|menu| menu.page == Page::Main) else {
            return;
        };
        if menu.capture.borrow().is_some() {
            menu.refresh_startup.set(true);
            return;
        }
        let button = GetFocus();
        let focus = (GetDlgCtrlID(button) as usize)
            .checked_sub(BUTTON)
            .and_then(|index| menu.rows.get(index))
            .and_then(|row| row.action);
        menu.page_transition(Page::Main, focus)
    };
    transition.run();
}

pub(super) fn close() -> bool {
    let hwnd = OPEN.replace(null_mut());
    !hwnd.is_null() && unsafe { DestroyWindow(hwnd) } != 0
}

pub(super) fn cancel_capture() {
    let hwnd = OPEN.get();
    if !hwnd.is_null() {
        unsafe { SendMessageW(hwnd, CANCEL_CAPTURE, 0, 0) };
    }
}

fn theme(hwnd: HWND, colors: Colors) {
    unsafe {
        for (attribute, value) in [
            (
                DWMWA_WINDOW_CORNER_PREFERENCE,
                if colors.high_contrast {
                    DWMWCP_DONOTROUND
                } else {
                    DWMWCP_ROUND
                } as u32,
            ),
            (DWMWA_USE_IMMERSIVE_DARK_MODE, u32::from(colors.dark)),
            (DWMWA_BORDER_COLOR, colors.border),
        ] {
            DwmSetWindowAttribute(
                hwnd,
                attribute as u32,
                (&raw const value).cast(),
                size_of_val(&value) as u32,
            );
        }
    }
}

pub(super) fn show(owner: HWND, point: POINT, prefs: &Prefs, keyboard: bool) {
    show_page(owner, point, prefs, keyboard, Page::Main, None);
}

fn show_page(
    owner: HWND,
    point: POINT,
    prefs: &Prefs,
    keyboard: bool,
    page: Page,
    focus: Option<Action>,
) {
    let result = unsafe { create(owner, point, prefs, keyboard, page, focus) };
    if let Err(error) = result {
        report(
            owner,
            &render(Msg::TrayOpenError, &[("error", &error.to_string())]),
        );
    }
}

unsafe fn create(
    owner: HWND,
    point: POINT,
    prefs: &Prefs,
    keyboard: bool,
    page: Page,
    focused: Option<Action>,
) -> std::io::Result<()> {
    unsafe {
        let instance = GetModuleHandleW(null());
        RegisterClassW(&WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(menu_proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: CLASS,
            ..Default::default()
        });
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut dpi = 96;
        let mut dpi_y = 96;
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
        let p = |n| scale(n, dpi);
        let work = info.rcWork;
        let rows = rows(prefs, page);
        let heights = rows
            .iter()
            .map(|row| {
                p(if row.action.is_none() { 43 } else { 34 }) + if row.separator { p(9) } else { 0 }
            })
            .collect::<Vec<_>>();
        let colors = Colors::current();
        let fonts = Fonts::new(dpi);
        let dc = GetDC(null_mut());
        let mut width = p(340);
        if !dc.is_null() {
            for row in &rows {
                let label = paint::text(
                    dc,
                    RECT::default(),
                    row.label,
                    fonts.regular,
                    0,
                    DT_CALCRECT,
                );
                let detail = paint::text(
                    dc,
                    RECT::default(),
                    &row.detail,
                    fonts.small,
                    0,
                    DT_CALCRECT,
                );
                width = width.max(
                    p(65)
                        + label
                        + if row.checked.is_some() {
                            p(44)
                        } else {
                            detail
                                + if row.action == Some(Action::Languages) {
                                    p(20)
                                } else {
                                    0
                                }
                        },
                );
            }
            ReleaseDC(null_mut(), dc);
        }
        let width = width.min(work.right - work.left);
        let visible_choices = if page == Page::Languages {
            (((work.bottom - work.top - p(12) - 2) / p(34)) - 1).clamp(1, CHOICE_PAGE_SIZE as i32)
                as usize
        } else {
            0
        };
        let ideal = if page == Page::Languages {
            p(34) * (visible_choices as i32 + 1)
        } else {
            heights.iter().sum()
        };
        let height = (ideal + p(12) + 2).min(work.bottom - work.top);
        let x = (point.x - width / 2).clamp(work.left, work.right - width);
        let above = point.y - height - p(5);
        let y = (if above >= work.top {
            above
        } else {
            point.y + p(5)
        })
        .clamp(work.top, work.bottom - height);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("Rustart"),
            WS_POPUP
                | WS_BORDER
                | if page == Page::Languages {
                    WS_VSCROLL
                } else {
                    0
                },
            x,
            y,
            width,
            height,
            owner,
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut icon = null_mut();
        LoadIconWithScaleDown(instance, super::icon_resource(), p(20), p(20), &mut icon);
        let menu = Box::new(Menu {
            owner,
            anchor: point,
            prefs: RefCell::new(prefs.clone()),
            page,
            rows,
            dpi,
            fonts,
            colors: Cell::new(colors),
            icon,
            hot: Cell::new(null_mut()),
            keyboard: Cell::new(keyboard),
            capture: RefCell::new(None),
            note: RefCell::new(String::new()),
            refresh_startup: Cell::new(false),
            scroll: Cell::new(0),
            wheel: Cell::new(0),
            visible_choices,
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(menu) as isize);
        OPEN.set(hwnd);
        theme(hwnd, colors);
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client);
        let mut top = p(6);
        let available = (client.bottom - p(12)).max(1);
        let menu = &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Menu);
        for (index, (row, height)) in menu.rows.iter().zip(heights).enumerate() {
            let height = height * available.min(ideal) / ideal;
            let button = CreateWindowExW(
                0,
                w!("BUTTON"),
                wide(&row.accessible_name()).as_ptr(),
                WS_CHILD
                    | if page == Page::Main { WS_VISIBLE } else { 0 }
                    | BS_OWNERDRAW as u32
                    | if row.action.is_some() && row.enabled {
                        WS_TABSTOP
                    } else {
                        WS_DISABLED
                    },
                p(6),
                top,
                client.right - p(12),
                height,
                hwnd,
                (BUTTON + index) as HMENU,
                instance,
                null(),
            );
            if button.is_null() {
                let error = std::io::Error::last_os_error();
                close();
                return Err(error);
            }
            SetWindowSubclass(button, Some(button_proc), 1, hwnd as usize);
            top += height;
        }
        if page == Page::Languages {
            menu.layout_choices(hwnd);
        }
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOSIZE | SWP_NOMOVE | SWP_SHOWWINDOW,
        );
        SetForegroundWindow(hwnd);
        let focus = menu
            .rows
            .iter()
            .position(|row| row.enabled && row.action.is_some() && row.action == focused)
            .or_else(|| {
                if focused == Some(Action::Admin) {
                    menu.rows
                        .iter()
                        .position(|row| row.enabled && row.action == Some(Action::Startup))
                } else {
                    None
                }
            })
            .unwrap_or(1);
        menu.focus(hwnd, focus);
    }
    Ok(())
}

impl Menu {
    fn hotkey_index(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.action == Some(Action::Hotkey))
    }

    fn display_row(&self, index: usize) -> Row {
        let mut row = self.rows[index].clone();
        if row.startup_mode.is_some() && startup_pending() {
            row.enabled = false;
        }
        if row.action == Some(Action::Hotkey) {
            row.note.clone_from(&self.note.borrow());
            let capture = self.capture.borrow();
            if let Some(capture) = capture.as_ref() {
                row.label = if row.note.is_empty() {
                    tr(Msg::TrayRecordHotkey)
                } else {
                    tr(Msg::TrayRetry)
                };
                row.detail = capture.keys.preview();
            } else {
                row.detail = capture::hotkey_label(self.prefs.borrow().hotkey);
            }
        }
        row
    }

    fn redraw_hotkey(&self, hwnd: HWND) {
        if let Some(index) = self.hotkey_index() {
            let row = self.display_row(index);
            let label = wide(&row.accessible_name());
            unsafe {
                let button = GetDlgItem(hwnd, (BUTTON + index) as i32);
                SetWindowTextW(button, label.as_ptr());
                InvalidateRect(button, null(), 0);
            }
        }
    }

    fn stop_recording(&self, hwnd: HWND) {
        let capture = self.capture.borrow_mut().take();
        if capture.is_some() {
            drop(capture);
            self.note.borrow_mut().clear();
            self.redraw_hotkey(hwnd);
            if self.refresh_startup.replace(false) {
                unsafe { PostMessageW(hwnd, STARTUP_FINISHED, 0, 0) };
            }
        }
    }

    fn start_recording(&self, hwnd: HWND, index: usize) {
        self.focus(hwnd, index);
        if unsafe { GetForegroundWindow() } != hwnd {
            return;
        }
        self.keyboard.set(true);
        self.note.borrow_mut().clear();
        match Capture::start(hwnd) {
            Ok(recording) => self.capture.replace(Some(recording)),
            Err(error) => {
                self.note.replace(render(
                    Msg::TrayHotkeyConflict,
                    &[("error", &error.to_string())],
                ));
                None
            }
        };
        self.redraw_hotkey(hwnd);
    }

    fn record_key(&self, hwnd: HWND, generation: usize, input: isize) {
        let focused = self.hotkey_index().is_some_and(|index| unsafe {
            GetFocus() == GetDlgItem(hwnd, (BUTTON + index) as i32)
        });
        if !focused {
            self.stop_recording(hwnd);
            return;
        }
        let event = {
            let mut capture = self.capture.borrow_mut();
            let Some(capture) = capture
                .as_mut()
                .filter(|recording| recording.generation == generation)
            else {
                return;
            };
            if Capture::take_delivery_failure() {
                capture.keys = Default::default();
                Event::Retry
            } else {
                capture.keys.handle(input as u16, (input >> 16) & 1 != 0)
            }
        };
        match event {
            Event::Cancel => {
                self.stop_recording(hwnd);
                return;
            }
            Event::Complete(hotkey) => match runtime::update_hotkey(hotkey) {
                Ok(()) => {
                    self.prefs.borrow_mut().hotkey = hotkey;
                    self.stop_recording(hwnd);
                    return;
                }
                Err(error) => {
                    self.note.replace(error);
                }
            },
            Event::Retry => {
                self.note.replace(tr(Msg::HotkeyInvalid).into());
            }
            Event::Changed => self.note.borrow_mut().clear(),
            Event::Unchanged => return,
        }
        self.redraw_hotkey(hwnd);
    }

    fn page_transition(&self, page: Page, focus: Option<Action>) -> Transition {
        Transition::Page {
            owner: self.owner,
            anchor: self.anchor,
            prefs: self.prefs.borrow().clone(),
            keyboard: self.keyboard.get(),
            page,
            focus,
        }
    }

    fn activate(&self, hwnd: HWND, index: usize) -> Transition {
        if self.capture.borrow().is_some() {
            self.stop_recording(hwnd);
            return Transition::Stay;
        }
        let Some(row) = self.rows.get(index).filter(|row| row.enabled) else {
            return Transition::Stay;
        };
        let Some(action) = row.action else {
            return Transition::Stay;
        };
        match action {
            Action::Hotkey => {
                self.start_recording(hwnd, index);
                Transition::Stay
            }
            Action::Languages => {
                let selected = self.prefs.borrow().language;
                self.page_transition(Page::Languages, Some(Action::ChooseLanguage(selected)))
            }
            Action::Back => self.page_transition(Page::Main, Some(Action::Languages)),
            Action::Startup | Action::Admin => {
                row.startup_mode
                    .map_or(Transition::Stay, |expected| Transition::Startup {
                        owner: self.owner,
                        expected,
                        desired: next_startup(expected, action == Action::Admin),
                    })
            }
            Action::ChooseLanguage(choice) => match runtime::update_language(choice) {
                Ok(()) => {
                    self.prefs.borrow_mut().language = choice;
                    self.page_transition(Page::Main, Some(Action::Languages))
                }
                Err(error) => Transition::Error(self.owner, error),
            },
            Action::Quit => Transition::Quit(self.owner),
        }
    }

    fn max_scroll(&self) -> usize {
        self.rows.len().saturating_sub(1 + self.visible_choices)
    }

    fn layout_choices(&self, hwnd: HWND) {
        unsafe {
            let mut area = RECT::default();
            GetClientRect(hwnd, &mut area);
            let pad = scale(6, self.dpi);
            let height = ((area.bottom - 2 * pad) / (self.visible_choices as i32 + 1)).max(1);
            let first = self.scroll.get() + 1;
            for index in 0..self.rows.len() {
                let button = GetDlgItem(hwnd, (BUTTON + index) as i32);
                if index == 0 || (first..first + self.visible_choices).contains(&index) {
                    let slot = if index == 0 { 0 } else { index - first + 1 };
                    SetWindowPos(
                        button,
                        null_mut(),
                        pad,
                        pad + slot as i32 * height,
                        (area.right - 2 * pad).max(1),
                        height,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                } else {
                    ShowWindow(button, SW_HIDE);
                }
            }
            let info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: self.rows.len() as i32 - 2,
                nPage: self.visible_choices as u32,
                nPos: self.scroll.get() as i32,
                ..Default::default()
            };
            SetScrollInfo(hwnd, SB_VERT, &info, 1);
        }
    }

    fn move_scroll(&self, hwnd: HWND, pos: i32) {
        let pos = pos.clamp(0, self.max_scroll() as i32) as usize;
        if pos != self.scroll.replace(pos) {
            self.layout_choices(hwnd);
        }
    }

    fn scroll_step(&self, hwnd: HWND, pos: i32) {
        // When moving the viewport by pointer, leave keyboard focus in the
        // visible pane instead of on an invisible native button.
        let focus = unsafe { GetFocus() };
        let index = unsafe { GetDlgCtrlID(focus) }
            .checked_sub(BUTTON as i32)
            .unwrap_or(0);
        self.move_scroll(hwnd, pos);
        if index > 0 {
            let first = self.scroll.get() + 1;
            self.focus(
                hwnd,
                (index as usize).clamp(first, first + self.visible_choices - 1),
            );
        }
    }

    fn focus(&self, hwnd: HWND, index: usize) {
        if self.page == Page::Languages && index != 0 {
            let first = self.scroll.get() + 1;
            if index < first {
                self.move_scroll(hwnd, index as i32 - 1);
            } else if index >= first + self.visible_choices {
                self.move_scroll(hwnd, (index - self.visible_choices) as i32);
            }
        }
        unsafe {
            let next = GetDlgItem(hwnd, (BUTTON + index) as i32);
            SetFocus(next);
            InvalidateRect(next, null(), 0);
        }
    }
}

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == STARTUP_FINISHED && OPEN.get() == hwnd {
        refresh_startup_menu();
        return 0;
    }
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Menu;
        let Some(menu) = ptr.as_ref() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        match msg {
            WM_COMMAND if lp != 0 && (wp >> 16) == BN_CLICKED as usize => {
                let index = (wp & 0xffff).saturating_sub(BUTTON);
                let transition = menu.activate(hwnd, index);
                transition.run();
                0
            }
            CANCEL_CAPTURE => {
                menu.stop_recording(hwnd);
                0
            }
            capture::KEY_EVENT => {
                if GetForegroundWindow() == hwnd {
                    menu.record_key(hwnd, wp, lp);
                } else {
                    menu.stop_recording(hwnd);
                }
                0
            }
            WM_MOUSEWHEEL if menu.page == Page::Languages => {
                let sum = menu.wheel.get() + i32::from((wp >> 16) as i16);
                let steps = sum / WHEEL_DELTA as i32;
                menu.wheel.set(sum % WHEEL_DELTA as i32);
                if steps != 0 {
                    menu.scroll_step(hwnd, menu.scroll.get() as i32 - steps * 3);
                }
                0
            }
            WM_VSCROLL if menu.page == Page::Languages => {
                let pos = menu.scroll.get() as i32;
                let page = menu.visible_choices as i32;
                let next = match (wp & 0xffff) as i32 {
                    SB_LINEUP => pos - 1,
                    SB_LINEDOWN => pos + 1,
                    SB_PAGEUP => pos - page,
                    SB_PAGEDOWN => pos + page,
                    SB_TOP => 0,
                    SB_BOTTOM => menu.max_scroll() as i32,
                    SB_THUMBTRACK | SB_THUMBPOSITION => {
                        let mut info = SCROLLINFO {
                            cbSize: size_of::<SCROLLINFO>() as u32,
                            fMask: SIF_TRACKPOS,
                            ..Default::default()
                        };
                        if GetScrollInfo(hwnd, SB_VERT, &mut info) != 0 {
                            info.nTrackPos
                        } else {
                            pos
                        }
                    }
                    _ => pos,
                };
                menu.scroll_step(hwnd, next);
                0
            }
            WM_DRAWITEM if lp != 0 => {
                let item = &*(lp as *const DRAWITEMSTRUCT);
                let index = (item.CtlID as usize).saturating_sub(BUTTON);
                if index < menu.rows.len() {
                    let row = menu.display_row(index);
                    Painter {
                        dpi: menu.dpi,
                        colors: menu.colors.get(),
                        fonts: &menu.fonts,
                        header_icon: menu.icon,
                        keyboard: menu.keyboard.get(),
                    }
                    .row(
                        item,
                        &row,
                        menu.hot.get() == item.hwndItem,
                        GetFocus() == item.hwndItem,
                    );
                }
                1
            }
            WM_ERASEBKGND => {
                let mut rect = RECT::default();
                GetClientRect(hwnd, &mut rect);
                paint::fill(wp as HDC, &rect, menu.colors.get().background, 0);
                1
            }
            WM_THEMECHANGED | WM_SETTINGCHANGE | WM_SYSCOLORCHANGE => {
                let colors = Colors::current();
                menu.colors.set(colors);
                theme(hwnd, colors);
                RedrawWindow(
                    hwnd,
                    null(),
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
                );
                0
            }
            WM_ACTIVATE if wp & 0xffff == WA_INACTIVE as usize => {
                menu.stop_recording(hwnd);
                // Destruction is deferred until the activation message unwinds.
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
                0
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_NCDESTROY => {
                if OPEN.get() == hwnd {
                    OPEN.set(null_mut());
                }
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn button_proc(
    button: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    parent: usize,
) -> LRESULT {
    unsafe {
        let hwnd = parent as HWND;
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Menu;
        if let Some(menu) = ptr.as_ref() {
            match msg {
                WM_MOUSEMOVE if menu.hot.get() != button => {
                    let previous = menu.hot.replace(button);
                    if menu.capture.borrow().is_none() {
                        menu.keyboard.set(false);
                    }
                    if !previous.is_null() {
                        InvalidateRect(previous, null(), 0);
                    }
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: button,
                        dwHoverTime: 0,
                    };
                    TrackMouseEvent(&mut track);
                    InvalidateRect(button, null(), 0);
                }
                WM_MOUSELEAVE if menu.hot.get() == button => {
                    menu.hot.set(null_mut());
                    InvalidateRect(button, null(), 0);
                }
                WM_MOUSEWHEEL if menu.page == Page::Languages => {
                    SendMessageW(hwnd, msg, wp, lp);
                    return 0;
                }
                WM_LBUTTONDOWN | WM_LBUTTONDBLCLK if menu.capture.borrow().is_some() => {
                    PostMessageW(hwnd, CANCEL_CAPTURE, 0, 0);
                    return 0;
                }
                WM_KILLFOCUS if menu.capture.borrow().is_some() => {
                    PostMessageW(hwnd, CANCEL_CAPTURE, 0, 0);
                }
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    if menu.capture.borrow().is_some() {
                        return 0;
                    }
                    let key = wp as u16;
                    if matches!(key, VK_ESCAPE | VK_LEFT | VK_BACK) {
                        if menu.page == Page::Languages {
                            let back = GetDlgItem(hwnd, BUTTON as i32);
                            PostMessageW(hwnd, WM_COMMAND, BUTTON, back as isize);
                        } else {
                            PostMessageW(hwnd, WM_CLOSE, 0, 0);
                        }
                        return 0;
                    }
                    let id = GetDlgCtrlID(button) as usize;
                    let index = id.saturating_sub(BUTTON);
                    if index >= menu.rows.len() {
                        return DefSubclassProc(button, msg, wp, lp);
                    }
                    if matches!(key, VK_RETURN | VK_SPACE)
                        || (key == VK_RIGHT && menu.rows[index].action == Some(Action::Languages))
                    {
                        if lp & (1 << 30) == 0 {
                            PostMessageW(hwnd, WM_COMMAND, id, button as isize);
                        }
                        return 0;
                    }
                    if menu.page == Page::Languages && matches!(key, VK_PRIOR | VK_NEXT) {
                        menu.keyboard.set(true);
                        let page = menu.visible_choices;
                        let destination = if key == VK_PRIOR {
                            index.saturating_sub(page)
                        } else {
                            (index + page).min(menu.rows.len() - 1)
                        };
                        menu.focus(hwnd, destination);
                        return 0;
                    }
                    if matches!(key, VK_UP | VK_DOWN | VK_TAB | VK_HOME | VK_END) {
                        menu.keyboard.set(true);
                        let rows = menu.rows.len();
                        let current = index;
                        let backwards = matches!(key, VK_UP | VK_END)
                            || (key == VK_TAB && GetKeyState(VK_SHIFT as i32) < 0);
                        let start = match key {
                            VK_HOME => rows - 1,
                            VK_END => rows,
                            _ => current,
                        };
                        for step in 1..=rows {
                            let index = if backwards {
                                (start + rows - step) % rows
                            } else {
                                (start + step) % rows
                            };
                            if menu.rows[index].enabled && menu.rows[index].action.is_some() {
                                menu.focus(hwnd, index);
                                break;
                            }
                        }
                        return 0;
                    }
                }
                _ => {}
            }
        }
        DefSubclassProc(button, msg, wp, lp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_startup_toggle_keeps_its_focus_target_and_reveals_the_right_switches() {
        let prefs = Prefs::default();
        let states = [
            (
                Mode::Off,
                false,
                Action::Startup,
                Mode::Standard,
                Some(false),
            ),
            (
                Mode::Standard,
                true,
                Action::Admin,
                Mode::Administrator,
                Some(true),
            ),
            (
                Mode::Administrator,
                true,
                Action::Admin,
                Mode::Standard,
                Some(false),
            ),
            (Mode::Administrator, false, Action::Startup, Mode::Off, None),
        ];
        for (before, administrator, focus, after, admin_checked) in states {
            let displayed = main_rows(&prefs, Ok(before));
            let clicked = displayed
                .iter()
                .find(|row| row.action == Some(focus))
                .unwrap();
            assert_eq!(clicked.startup_mode, Some(before));
            assert_eq!(next_startup(before, administrator), after);
            let rows = main_rows(&prefs, Ok(after));
            assert!(
                rows.iter()
                    .any(|row| row.action == Some(focus) && row.enabled)
            );
            let startup = rows
                .iter()
                .find(|row| row.action == Some(Action::Startup))
                .unwrap();
            assert_eq!(startup.checked, Some(after != Mode::Off));
            let admin = rows.iter().find(|row| row.action == Some(Action::Admin));
            assert_eq!(admin.and_then(|row| row.checked), admin_checked);
        }
        assert_eq!(
            next_startup(Mode::DisabledByWindows, false),
            Mode::DisabledByWindows
        );
    }

    #[test]
    fn startup_completion_is_consumed_once_even_with_no_popup() {
        let (sender, receiver) = mpsc::channel();
        STARTUP_CHANGE.set(Some(StartupChange {
            expected: Mode::Standard,
            result: receiver,
        }));
        finish_startup_change(null_mut());
        assert!(startup_pending());
        sender.send(Ok(())).unwrap();
        finish_startup_change(null_mut());
        assert!(!startup_pending());
        finish_startup_change(null_mut());
        assert!(!startup_pending());
    }

    #[test]
    fn language_page_lists_system_and_all_native_names_and_marks_one_choice() {
        for language in std::iter::once(None).chain(Language::ALL.into_iter().map(Some)) {
            let choices = rows(
                &Prefs {
                    language,
                    ..Prefs::default()
                },
                Page::Languages,
            );
            assert_eq!(choices.len(), Language::ALL.len() + 2);
            assert!(choices[0].action == Some(Action::Back));
            assert!(choices[1].action == Some(Action::ChooseLanguage(None)));
            for (row, expected) in choices[2..].iter().zip(Language::ALL) {
                assert_eq!(row.label, expected.native_name());
            }
            let selected = choices
                .iter()
                .filter(|row| row.checked == Some(true))
                .collect::<Vec<_>>();
            assert_eq!(selected.len(), 1);
            assert!(selected[0].action == Some(Action::ChooseLanguage(language)));
        }
    }
}
