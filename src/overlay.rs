mod buttons;
mod icons;
pub(crate) use icons::run_worker as run_icon_worker;
mod raster;
mod shell_action;
mod view;

use std::{
    cell::{Cell, RefCell},
    ptr::null_mut,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_CLASS_ALREADY_EXISTS, GetLastError, GlobalFree, HWND, LPARAM, LRESULT, RECT,
            SetLastError, WPARAM,
        },
        Graphics::Gdi::*,
        System::{
            DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
            LibraryLoader::GetModuleHandleW,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        },
        UI::{
            Controls::{
                DRAWITEMSTRUCT, EM_EMPTYUNDOBUFFER, EM_LIMITTEXT, EM_SETCUEBANNER, WM_MOUSELEAVE,
            },
            Input::KeyboardAndMouse::*,
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

use crate::{
    calc,
    catalog::{self, App, Source},
    i18n::{self, ControlLabel, Msg, render, tr},
};
pub(crate) use shell_action::run_worker as run_shell_action_worker;
use shell_action::{ShellAction, ShellOperation};
use view::{Appearance, Geometry};

const CLASS: *const u16 = w!("Rustart.Overlay");
const EDIT_ID: usize = 101;
const LIST_ID: usize = 102;
const SHOW: u32 = WM_APP + 1;
const HIDE: u32 = WM_APP + 2;
const ACTIVATE: u32 = WM_APP + 7;
const CHECK_FOCUS: u32 = WM_APP + 8;
const REFRESH: u32 = WM_APP + 9;
const SHELL_DONE: u32 = WM_APP + 10;
const LANGUAGE_CHANGED: u32 = WM_APP + 11;
const VISIBLE_ROWS: usize = 3;
const RELEASE_VIEW_TIMER: usize = 1;
const RELEASE_VIEW_DELAY_MS: u32 = 2_000;
const CLIPBOARD_UNICODE_TEXT: u32 = 13;
const MAX_SHELL_REQUESTS: usize = 3;

struct State {
    edit: Cell<HWND>,
    list: Cell<HWND>,
    buttons: buttons::Buttons,
    icons: RefCell<icons::Loader>,
    icon_px: Cell<i32>,
    apps: RefCell<Vec<App>>,
    results: RefCell<Results>,
    app_query_active: Cell<bool>,
    appearance: RefCell<Appearance>,
    release_view_at: Cell<Option<Instant>>,
    geometry: Cell<Geometry>,
    shape: Cell<(i32, i32, i32)>,
    previous: Cell<HWND>,
    composing: Cell<bool>,
    wheel: Cell<i32>,
    refreshing: Cell<bool>,
    revision: Cell<u64>,
    view_top: Cell<usize>,
    button_press: Cell<Option<u64>>,
    discard_button_click: Cell<bool>,
    running_action: Cell<bool>,
    pending_action: RefCell<Option<(Interaction, Action)>>,
    session: Cell<u64>,
    interaction: Cell<u64>,
    pending_shell: RefCell<Vec<ShellRequest>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Interaction {
    session: u64,
    revision: u64,
}

struct ShellRequest {
    origin: Interaction,
    operation: ShellOperation,
    outcome: Receiver<Result<(), String>>,
}

enum Action {
    Copy(String),
    Shell(ShellAction),
}

#[derive(Debug, PartialEq, Eq)]
enum ActionShortcut {
    Open,
    Elevated,
    Repeat,
}

fn decode_action_key(
    key: u16,
    ctrl: bool,
    shift: bool,
    alt: bool,
    previous_state: bool,
) -> Option<ActionShortcut> {
    if key != VK_RETURN || alt || ctrl != shift {
        return None;
    }
    Some(if previous_state {
        ActionShortcut::Repeat
    } else if ctrl {
        ActionShortcut::Elevated
    } else {
        ActionShortcut::Open
    })
}

fn visible_top(top: usize, selected: usize, count: usize) -> usize {
    let rows = count.min(VISIBLE_ROWS);
    top.clamp((selected + 1).saturating_sub(rows), selected)
        .min(count - rows)
}

fn ensure_selection_visible(state: &State) {
    let count = state.count();
    let list = state.list.get();
    unsafe {
        let selected = SendMessageW(list, LB_GETCURSEL, 0, 0) as usize;
        if selected < count {
            let top = SendMessageW(list, LB_GETTOPINDEX, 0, 0).max(0) as usize;
            let visible = visible_top(top, selected, count);
            if visible != top {
                SendMessageW(list, LB_SETTOPINDEX, visible, 0);
            }
        }
    }
}

#[derive(Default, PartialEq, Eq)]
enum Results {
    #[default]
    Empty,
    Apps(Vec<usize>),
    Calculation(String),
}

impl State {
    fn new(hwnd: HWND) -> Self {
        Self {
            edit: Cell::new(null_mut()),
            list: Cell::new(null_mut()),
            buttons: buttons::Buttons::new(),
            icons: RefCell::new(icons::Loader::new(hwnd)),
            icon_px: Cell::new(0),
            apps: RefCell::new(Vec::new()),
            results: RefCell::new(Results::Empty),
            app_query_active: Cell::new(false),
            appearance: RefCell::new(Appearance::new(96)),
            release_view_at: Cell::new(None),
            geometry: Cell::new(Geometry::default()),
            shape: Cell::default(),
            previous: Cell::new(null_mut()),
            composing: Cell::new(false),
            wheel: Cell::new(0),
            refreshing: Cell::new(false),
            revision: Cell::new(0),
            view_top: Cell::new(0),
            button_press: Cell::new(None),
            discard_button_click: Cell::new(false),
            running_action: Cell::new(false),
            pending_action: RefCell::default(),
            session: Cell::new(0),
            interaction: Cell::new(0),
            pending_shell: RefCell::default(),
        }
    }

    fn count(&self) -> usize {
        match &*self.results.borrow() {
            Results::Apps(indices) => indices.len(),
            Results::Calculation(_) => 1,
            _ => 0,
        }
    }

    fn request_icons(&self, top: usize) {
        let px = self.geometry.get().px(view::APP_ICON_DIP);
        let mut loader = self.icons.borrow_mut();
        if self.icon_px.replace(px) != px {
            loader.clear();
        }
        let results = self.results.borrow();
        let Results::Apps(indices) = &*results else {
            loader.request(std::iter::empty(), px);
            return;
        };
        let apps = self.apps.borrow();
        let visible = indices
            .iter()
            .skip(top)
            .take(VISIBLE_ROWS)
            .filter_map(|&index| apps.get(index).map(|app| &app.source));
        loader.request(visible, px);
    }

    fn app_icon(&self, index: usize) -> HICON {
        self.apps
            .borrow()
            .get(index)
            .map(|app| self.icons.borrow().get(&app.source, self.icon_px.get()))
            .unwrap_or(null_mut())
    }

    fn is_calculation(&self) -> bool {
        matches!(*self.results.borrow(), Results::Calculation(_))
    }

    fn renew_rows(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
    }

    fn current_interaction(&self) -> Interaction {
        Interaction {
            session: self.session.get(),
            revision: self.interaction.get(),
        }
    }

    fn renew_interaction(&self) {
        self.interaction.set(self.interaction.get().wrapping_add(1));
    }
}

pub fn create(owner: HWND) -> Result<HWND, String> {
    unsafe {
        let module = GetModuleHandleW(null_mut());
        let class = WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(window_proc),
            hInstance: module,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: CLASS,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_CLASS_ALREADY_EXISTS as i32) {
                return Err(render(
                    Msg::OverlayRegisterFailed,
                    &[("error", &error.to_string())],
                ));
            }
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("Rustart"),
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            0,
            0,
            owner,
            null_mut(),
            module,
            null_mut(),
        );
        if hwnd.is_null() {
            Err(render(
                Msg::OverlayCreateFailed,
                &[("error", &std::io::Error::last_os_error().to_string())],
            ))
        } else {
            Ok(hwnd)
        }
    }
}

pub fn show(hwnd: HWND) {
    unsafe {
        SendMessageW(hwnd, SHOW, 0, 0);
    }
}

pub fn hide(hwnd: HWND) {
    unsafe {
        SendMessageW(hwnd, HIDE, 0, 0);
    }
}

pub fn refresh_language(hwnd: HWND) {
    unsafe {
        SendMessageW(hwnd, LANGUAGE_CHANGED, 0, 0);
    }
}

pub(crate) fn showing_app_search(hwnd: HWND) -> bool {
    if hwnd.is_null() || unsafe { IsWindowVisible(hwnd) } == 0 {
        return false;
    }
    // Called on the owning UI thread, just like the catalog setter below.
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const State;
    unsafe { state.as_ref() }.is_some_and(|state| state.app_query_active.get())
}

pub fn set_catalog(hwnd: HWND, apps: Vec<App>) {
    // The runtime publishes on the overlay's UI thread.
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
        if let Some(state) = ptr.as_ref() {
            // A shortcut's icon can change without changing its catalog entry.
            state.icons.borrow_mut().catalog_changed();
            if *state.apps.borrow() == apps {
                return;
            }
            let visible = IsWindowVisible(hwnd) != 0;
            let selected = if visible {
                selected_source(state)
            } else {
                None
            };
            // Child messages can repaint while the native list is rebuilt.
            // Old result indices belong to the old catalog.
            state.results.replace(Results::Empty);
            state.apps.replace(apps);
            if visible {
                refresh_results(hwnd, state, selected.as_ref());
            }
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if message == WM_CREATE {
            if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != 0 {
                return 0;
            }
            let ptr = Box::into_raw(Box::new(State::new(hwnd)));
            SetLastError(0);
            if SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize) == 0 {
                let error = GetLastError();
                if error != 0 {
                    drop(Box::from_raw(ptr));
                    SetLastError(error);
                    return -1;
                }
            }
            // From here, WM_NCDESTROY owns cleanup even if a child fails to
            // initialize and WM_CREATE rejects the window.
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, message, wp, lp);
        }
        if message == WM_NCDESTROY {
            KillTimer(hwnd, RELEASE_VIEW_TIMER);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr));
            return DefWindowProcW(hwnd, message, wp, lp);
        }

        // Shared access survives synchronous child-control notifications, including
        // owner-draw callbacks. RefCell guards never span calls into those controls.
        let state = &*ptr;
        match message {
            WM_CREATE => return if create_controls(hwnd, state) { 0 } else { -1 },
            WM_SETFOCUS if !state.edit.get().is_null() => {
                SetFocus(state.edit.get());
            }
            SHOW => show_window(hwnd, state),
            HIDE | WM_CLOSE => hide_window(hwnd, state, true),
            LANGUAGE_CHANGED => {
                let edit = state.edit.get();
                SendMessageW(
                    edit,
                    EM_SETCUEBANNER,
                    1,
                    i18n::wide(ControlLabel::OverlayCue) as LPARAM,
                );
                SetWindowTextW(state.list.get(), i18n::wide(ControlLabel::OverlayResults));
                state.buttons.refresh_language(state.list.get());
                InvalidateRect(edit, null_mut(), 0);
            }
            WM_TIMER if wp == RELEASE_VIEW_TIMER => {
                if state
                    .release_view_at
                    .get()
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    cancel_view_release(hwnd, state);
                    release_hidden_view(hwnd, state);
                }
            }
            CHECK_FOCUS => {
                if !state.running_action.get() && GetForegroundWindow() != hwnd {
                    hide_window(hwnd, state, false);
                }
            }
            WM_ACTIVATE if wp as u16 == WA_INACTIVE as u16 && !state.running_action.get() => {
                PostMessageW(hwnd, CHECK_FOCUS, 0, 0);
            }
            WM_COMMAND
                if wp as u16 as usize == EDIT_ID && (wp >> 16) as u16 as u32 == EN_CHANGE =>
            {
                if !state.refreshing.get() && IsWindowVisible(hwnd) != 0 {
                    state.renew_interaction();
                    refresh_results(hwnd, state, None);
                }
                if view::recolor_edit_cue(state) && GetWindowTextLengthW(state.edit.get()) == 0 {
                    InvalidateRect(state.edit.get(), null_mut(), 0);
                }
            }
            WM_COMMAND if wp as u16 as usize == LIST_ID => match (wp >> 16) as u16 as u32 {
                LBN_DBLCLK => queue_selected(hwnd, state, buttons::Kind::Open),
                LBN_SELCHANGE => {
                    state.renew_interaction();
                    view::layout_buttons(hwnd, state);
                }
                _ => (),
            },
            WM_COMMAND if buttons::is_button(wp as u16 as usize) => {
                if (wp >> 16) as u16 as u32 == BN_CLICKED {
                    button_click(hwnd, state, wp as u16 as usize, lp as HWND);
                }
            }
            ACTIVATE => run_action(hwnd, state),
            SHELL_DONE => finish_shell(hwnd, state),
            icons::READY => {
                let updated = state.icons.borrow_mut().collect();
                if updated && IsWindowVisible(hwnd) != 0 {
                    InvalidateRect(state.list.get(), null_mut(), 0);
                }
            }
            WM_MOUSEWHEEL => scroll(hwnd, state, wp),
            WM_LBUTTONDOWN => {
                if view::hits_input(state, lp) {
                    SetFocus(state.edit.get());
                }
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED | WM_THEMECHANGED | WM_SYSCOLORCHANGE
            | WM_SETTINGCHANGE => {
                PostMessageW(hwnd, REFRESH, 0, 0);
            }
            REFRESH => {
                if IsWindowVisible(hwnd) != 0 {
                    state.geometry.set(Geometry {
                        area: work_area(hwnd),
                        ..state.geometry.get()
                    });
                    view::update_appearance(hwnd, state);
                    view::layout(hwnd, state);
                }
            }
            WM_PAINT => {
                if IsWindowVisible(hwnd) != 0 {
                    view::paint(hwnd, state);
                } else {
                    ValidateRect(hwnd, null_mut());
                }
            }
            WM_DRAWITEM if wp == LIST_ID && IsWindowVisible(hwnd) != 0 => {
                if let Some(item) = (lp as *const DRAWITEMSTRUCT).as_ref()
                    && item.hwndItem == state.list.get()
                {
                    view::draw_row(state, item);
                    return 1;
                }
            }
            WM_CTLCOLOREDIT => return view::edit_colors(wp as HDC, state) as LRESULT,
            WM_CTLCOLORLISTBOX => return view::list_brush(state) as LRESULT,
            WM_ERASEBKGND => return 1,
            _ => return DefWindowProcW(hwnd, message, wp, lp),
        }
        0
    }
}

fn create_controls(hwnd: HWND, state: &State) -> bool {
    unsafe {
        let module = GetModuleHandleW(null_mut());
        let edit = CreateWindowExW(
            0,
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            hwnd,
            EDIT_ID as _,
            module,
            null_mut(),
        );
        if edit.is_null()
            || SetWindowSubclass(edit, Some(control_proc), EDIT_ID, hwnd as usize) == 0
        {
            return false;
        }
        state.edit.set(edit);
        SendMessageW(edit, EM_LIMITTEXT, 512, 0);
        SendMessageW(
            edit,
            EM_SETCUEBANNER,
            1,
            i18n::wide(ControlLabel::OverlayCue) as LPARAM,
        );

        let list = CreateWindowExW(
            0,
            w!("LISTBOX"),
            i18n::wide(ControlLabel::OverlayResults),
            WS_CHILD
                | WS_CLIPSIBLINGS
                | LBS_NOTIFY as u32
                | LBS_OWNERDRAWFIXED as u32
                | LBS_HASSTRINGS as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            0,
            0,
            0,
            0,
            hwnd,
            LIST_ID as _,
            module,
            null_mut(),
        );
        if list.is_null()
            || SetWindowSubclass(list, Some(control_proc), LIST_ID, hwnd as usize) == 0
        {
            return false;
        }
        state.list.set(list);
        state.buttons.create(hwnd, list, module)
    }
}

fn show_window(hwnd: HWND, state: &State) {
    cancel_view_release(hwnd, state);
    unsafe {
        // An explicit show starts a new interaction even when Explorer has
        // opened but the previous folder request has not posted completion.
        state.session.set(state.session.get().wrapping_add(1));
        state.pending_action.replace(None);
        if IsWindowVisible(hwnd) != 0 {
            SetForegroundWindow(hwnd);
            SetFocus(state.edit.get());
            view::layout(hwnd, state);
            return;
        }
        let foreground = GetForegroundWindow();
        state.previous.set(foreground);
        state.composing.set(false);
        state.wheel.set(0);
        let area = work_area(foreground);
        state.geometry.set(Geometry { area, dpi: 96 });
        // Move the hidden window to the destination display first so the edit
        // and all geometry use the same DPI context as the final popup.
        SetWindowPos(
            hwnd,
            null_mut(),
            (area.left + area.right) / 2,
            (area.top + area.bottom) / 2,
            1,
            1,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        view::update_appearance(hwnd, state);
        state.refreshing.set(true);
        SetWindowTextW(state.edit.get(), w!(""));
        state.refreshing.set(false);
        refresh_results(hwnd, state, None);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(state.edit.get());
        // Initialize the whole popup after it becomes visible. Its layered
        // surface can otherwise contain only the edit's first dirty rectangle.
        RedrawWindow(
            hwnd,
            null_mut(),
            null_mut(),
            RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
        );
    }
}

fn work_area(foreground: HWND) -> RECT {
    unsafe {
        let monitor = if foreground.is_null() {
            let mut point = Default::default();
            GetCursorPos(&mut point);
            MonitorFromPoint(point, MONITOR_DEFAULTTOPRIMARY)
        } else {
            MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST)
        };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            info.rcWork
        } else {
            let mut area = RECT::default();
            SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut area).cast(), 0);
            area
        }
    }
}

fn hide_window(hwnd: HWND, state: &State, restore: bool) {
    unsafe {
        state.pending_action.replace(None);
        state.button_press.set(None);
        state
            .icons
            .borrow_mut()
            .request(std::iter::empty(), state.icon_px.get());
        state.buttons.hot.set(null_mut());
        if IsWindowVisible(hwnd) == 0 {
            return;
        }
        let previous = state.previous.replace(null_mut());
        let restore = restore && GetForegroundWindow() == hwnd;
        ShowWindow(hwnd, SW_HIDE);
        if restore && IsWindow(previous) != 0 {
            SetForegroundWindow(previous);
        }
    }
    schedule_view_release(hwnd, state);
}

fn schedule_view_release(hwnd: HWND, state: &State) {
    if unsafe { IsWindowVisible(hwnd) } != 0 {
        return;
    }
    let delay = Duration::from_millis(u64::from(RELEASE_VIEW_DELAY_MS));
    state.release_view_at.set(Some(Instant::now() + delay));
    if unsafe { SetTimer(hwnd, RELEASE_VIEW_TIMER, RELEASE_VIEW_DELAY_MS, None) } == 0 {
        state.release_view_at.set(None);
        release_hidden_view(hwnd, state);
    }
}

fn cancel_view_release(hwnd: HWND, state: &State) {
    if state.release_view_at.take().is_some() {
        unsafe {
            KillTimer(hwnd, RELEASE_VIEW_TIMER);
        }
    }
}

fn release_hidden_view(hwnd: HWND, state: &State) {
    if unsafe { IsWindowVisible(hwnd) } != 0 {
        return;
    }
    state.icons.borrow_mut().release();
    state.icon_px.set(0);
    state.results.replace(Results::Empty);
    state.app_query_active.set(false);
    state.renew_rows();
    state.view_top.set(0);
    state.refreshing.set(true);
    unsafe {
        // Keep the HWNDs stable, but discard native list strings and edit
        // text/undo data along with the Rust result model.
        SendMessageW(state.list.get(), LB_RESETCONTENT, 0, 0);
        let edit = state.edit.get();
        SetWindowTextW(edit, w!(""));
        SendMessageW(edit, EM_EMPTYUNDOBUFFER, 0, 0);
    }
    state.refreshing.set(false);
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        view::release_renderer(state);
        crate::runtime::overlay_idle();
    }
}

fn selected_source(state: &State) -> Option<Source> {
    let selected = unsafe { SendMessageW(state.list.get(), LB_GETCURSEL, 0, 0) };
    let results = state.results.borrow();
    let Results::Apps(indices) = &*results else {
        return None;
    };
    let index = *indices.get(selected as usize)?;
    Some(state.apps.borrow().get(index)?.source.clone())
}

fn refresh_results(hwnd: HWND, state: &State, previous: Option<&Source>) {
    unsafe {
        let edit = state.edit.get();
        let list = state.list.get();
        let mut value = vec![0; GetWindowTextLengthW(edit) as usize + 1];
        let length = GetWindowTextW(edit, value.as_mut_ptr(), value.len() as i32);
        let query = String::from_utf16_lossy(&value[..length as usize]);
        let query = query.trim();

        let results = if query.is_empty() {
            Results::Empty
        } else if let Some(result) = calc::evaluate(query) {
            match result {
                Ok(value) => Results::Calculation(value),
                Err(_) => Results::Empty,
            }
        } else {
            Results::Apps(catalog::search(&state.apps.borrow(), query))
        };
        let selected = match (&results, previous) {
            (Results::Apps(indices), Some(source)) => {
                let apps = state.apps.borrow();
                indices
                    .iter()
                    .position(|&index| &apps[index].source == source)
                    .unwrap_or(0)
            }
            _ => 0,
        };
        let app_query = matches!(results, Results::Apps(_));
        let search_started = !state.app_query_active.replace(app_query) && app_query;
        let old_count = SendMessageW(list, LB_GETCOUNT, 0, 0).max(0) as usize;
        let changed = results != *state.results.borrow() || old_count != state.count();
        let reset_selection =
            old_count != 0 && SendMessageW(list, LB_GETCURSEL, 0, 0) != selected as isize;
        state.renew_rows();
        if changed {
            let names: Vec<_> = match &results {
                Results::Apps(indices) => {
                    let apps = state.apps.borrow();
                    indices.iter().map(|&i| wide(&apps[i].name)).collect()
                }
                Results::Calculation(value) => vec![wide(value)],
                Results::Empty => Vec::new(),
            };
            state.results.replace(results);
            // Enabling WM_SETREDRAW on a hidden list would make it visible
            // before layout has positioned it for the new results.
            let visible = IsWindowVisible(list) != 0;
            if visible {
                SendMessageW(list, WM_SETREDRAW, 0, 0);
            }
            SendMessageW(list, LB_RESETCONTENT, 0, 0);
            for name in &names {
                SendMessageW(list, LB_ADDSTRING, 0, name.as_ptr() as LPARAM);
            }
            if !names.is_empty() {
                SendMessageW(list, LB_SETCURSEL, selected, 0);
            }
            if visible {
                SendMessageW(list, WM_SETREDRAW, 1, 0);
            }
            InvalidateRect(list, null_mut(), 0);
        } else if reset_selection {
            select_result(state, selected);
        }
        let count = state.count();
        // The first three results add height. The fourth reserves the scroll gutter.
        if IsWindowVisible(hwnd) == 0
            || old_count.min(VISIBLE_ROWS + 1) != count.min(VISIBLE_ROWS + 1)
        {
            view::layout(hwnd, state);
        } else if changed || reset_selection {
            ensure_selection_visible(state);
            view::layout_buttons(hwnd, state);
            if old_count != count {
                view::invalidate_rail(hwnd, state);
            }
        }
        if search_started {
            crate::runtime::search_started(hwnd);
        }
    }
}

fn queue_selected(hwnd: HWND, state: &State, kind: buttons::Kind) {
    let selected = unsafe { SendMessageW(state.list.get(), LB_GETCURSEL, 0, 0) };
    if let Ok(row) = usize::try_from(selected) {
        queue_action(hwnd, state, row, kind);
    }
}

fn queue_action(hwnd: HWND, state: &State, row: usize, kind: buttons::Kind) {
    if state.running_action.get() || state.pending_action.borrow().is_some() {
        return;
    }
    let action = action_for(&state.results.borrow(), &state.apps.borrow(), row, kind);
    if let Some(action) = action {
        state
            .pending_action
            .replace(Some((state.current_interaction(), action)));
        if unsafe { PostMessageW(hwnd, ACTIVATE, 0, 0) } == 0 {
            state.pending_action.replace(None);
        }
    }
}

fn action_for(results: &Results, apps: &[App], row: usize, kind: buttons::Kind) -> Option<Action> {
    match results {
        Results::Calculation(value)
            if row == 0 && matches!(kind, buttons::Kind::Copy | buttons::Kind::Open) =>
        {
            Some(Action::Copy(value.clone()))
        }
        Results::Apps(indices) => {
            let app = apps.get(*indices.get(row)?)?;
            let operation = match kind {
                buttons::Kind::Open => ShellOperation::Launch(false),
                buttons::Kind::Elevated => ShellOperation::Launch(true),
                buttons::Kind::Folder => ShellOperation::Reveal,
                buttons::Kind::Copy => return None,
            };
            Some(Action::Shell(ShellAction::new(app, operation)))
        }
        _ => None,
    }
}

fn button_click(hwnd: HWND, state: &State, id: usize, clicked: HWND) {
    unsafe {
        if state.discard_button_click.get()
            || clicked.is_null()
            || state.buttons.get(id) != Some(clicked)
            || IsWindowVisible(clicked) == 0
        {
            return;
        }
        let (slot, kind) = buttons::id_action(id);
        queue_action(hwnd, state, view::top_index(state) + slot, kind);
    }
}

fn run_action(hwnd: HWND, state: &State) {
    let Some((origin, action)) = state.pending_action.take() else {
        return;
    };
    if origin != state.current_interaction() || unsafe { IsWindowVisible(hwnd) } == 0 {
        return;
    }
    let result = match action {
        Action::Copy(value) => {
            state.running_action.set(true);
            let copied = copy_text(hwnd, &value);
            state.running_action.set(false);
            if copied {
                hide_window(hwnd, state, true);
                return;
            }
            Err(tr(Msg::OverlayCopyFailed).into())
        }
        Action::Shell(action) => start_shell(hwnd, state, origin, action),
    };
    if let Err(error) = result {
        crate::diagnostic(error);
    }
}

fn start_shell(
    hwnd: HWND,
    state: &State,
    origin: Interaction,
    action: ShellAction,
) -> Result<(), String> {
    let operation = action.operation;
    {
        let pending = state.pending_shell.borrow();
        if pending.iter().any(|request| request.origin == origin) {
            return Ok(());
        }
        if pending.len() == MAX_SHELL_REQUESTS {
            return Err(tr(Msg::OverlayActionBusy).into());
        }
    }
    let (tx, outcome) = mpsc::sync_channel(1);
    let owner = hwnd as usize;
    thread::Builder::new()
        .name("rustart-shell".into())
        .spawn(move || {
            let _ = tx.send(action.execute_isolated());
            unsafe {
                PostMessageW(owner as HWND, SHELL_DONE, 0, 0);
            }
        })
        .map_err(|error| format!("{} {error}", operation.failure()))?;
    state.pending_shell.borrow_mut().push(ShellRequest {
        origin,
        operation,
        outcome,
    });
    Ok(())
}

fn finish_shell(hwnd: HWND, state: &State) {
    let current = state.current_interaction();
    let mut completed = None;
    state.pending_shell.borrow_mut().retain(|request| {
        let result = match request.outcome.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => Err(request.operation.failure().into()),
        };
        if request.origin == current {
            completed = Some(result);
        }
        false
    });
    let Some(result) = completed else {
        return;
    };
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return;
    }
    match result {
        Ok(()) => hide_window(hwnd, state, false),
        Err(error) => crate::diagnostic(error),
    }
}

fn navigate(hwnd: HWND, state: &State, delta: isize) {
    let count = state.count();
    if count == 0 {
        return;
    }
    unsafe {
        let list = state.list.get();
        // After moving the row selection, Enter should open that row, even
        // when navigation started on a secondary action reached with Tab.
        let focus = GetFocus();
        if state.buttons.iter().any(|(_, button)| button == focus) {
            SetFocus(state.edit.get());
        }
        let selected = SendMessageW(list, LB_GETCURSEL, 0, 0).max(0);
        let next = (selected + delta).clamp(0, count as isize - 1);
        if next == selected {
            return;
        }
        state.renew_interaction();
        select_result(state, next as usize);
        view::layout_buttons(hwnd, state);
    }
}

fn select_result(state: &State, index: usize) {
    let list = state.list.get();
    unsafe {
        // LISTBOX normally scrolls pixels already on screen and draws the two
        // selections synchronously. Present the new viewport in one WM_PAINT.
        SendMessageW(list, WM_SETREDRAW, 0, 0);
        SendMessageW(list, LB_SETCURSEL, index, 0);
        ensure_selection_visible(state);
        SendMessageW(list, WM_SETREDRAW, 1, 0);
        InvalidateRect(list, null_mut(), 0);
    }
}

fn scroll(hwnd: HWND, state: &State, wp: WPARAM) {
    let total = state.wheel.get() + i32::from((wp >> 16) as i16);
    state.wheel.set(total % WHEEL_DELTA as i32);
    let steps = total / WHEEL_DELTA as i32;
    if steps != 0 {
        navigate(hwnd, state, -steps as isize);
    }
}

unsafe extern "system" fn control_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    parent: usize,
) -> LRESULT {
    unsafe {
        let parent = parent as HWND;
        let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *const State;
        if message == WM_NCDESTROY {
            RemoveWindowSubclass(hwnd, Some(control_proc), id);
        }
        if let Some(state) = ptr.as_ref() {
            if let Some(result) = buttons::paint_message(state, hwnd, id, message, wp, lp) {
                return result;
            }
            match message {
                WM_ERASEBKGND if id == LIST_ID => return 1,
                WM_PAINT if id == LIST_ID && wp == 0 => {
                    view::paint_list(hwnd, state);
                    return 0;
                }
                WM_COMMAND if id == LIST_ID && buttons::is_button(wp as u16 as usize) => {
                    return SendMessageW(parent, WM_COMMAND, wp, lp);
                }
                WM_MOUSEMOVE if buttons::is_button(id) => state.buttons.hover(hwnd),
                WM_MOUSELEAVE if buttons::is_button(id) => state.buttons.leave(hwnd),
                WM_LBUTTONDOWN | WM_LBUTTONDBLCLK if buttons::is_button(id) => {
                    state.button_press.set(Some(state.revision.get()));
                }
                WM_LBUTTONUP if buttons::is_button(id) => {
                    // Native BUTTON emits BN_CLICKED synchronously while it
                    // processes this release. A scrolled slot may have changed.
                    let stale = state
                        .button_press
                        .take()
                        .is_some_and(|revision| revision != state.revision.get());
                    let previous = state.discard_button_click.replace(stale);
                    let result = DefSubclassProc(hwnd, message, wp, lp);
                    state.discard_button_click.set(previous);
                    return result;
                }
                WM_SETFOCUS | WM_KILLFOCUS if id == EDIT_ID => {
                    InvalidateRect(parent, null_mut(), 0);
                    if view::recolor_edit_cue(state) && GetWindowTextLengthW(hwnd) == 0 {
                        InvalidateRect(hwnd, null_mut(), 0);
                    }
                }
                WM_PAINT | WM_PRINTCLIENT if id == EDIT_ID && view::recolor_edit_cue(state) => {
                    let result = DefSubclassProc(hwnd, message, wp, lp);
                    if !state.composing.get() && GetWindowTextLengthW(hwnd) == 0 {
                        let target = wp as HDC;
                        if !target.is_null() {
                            view::draw_dark_edit_cue(hwnd, target, state);
                        } else if message == WM_PAINT {
                            let dc = GetDC(hwnd);
                            if !dc.is_null() {
                                let caret_hidden = GetFocus() == hwnd && HideCaret(hwnd) != 0;
                                view::draw_dark_edit_cue(hwnd, dc, state);
                                if caret_hidden {
                                    ShowCaret(hwnd);
                                }
                                ReleaseDC(hwnd, dc);
                            }
                        }
                    }
                    return result;
                }
                WM_IME_STARTCOMPOSITION | WM_IME_ENDCOMPOSITION => {
                    state.composing.set(message == WM_IME_STARTCOMPOSITION);
                    if id == EDIT_ID && view::recolor_edit_cue(state) {
                        InvalidateRect(hwnd, null_mut(), 0);
                    }
                }
                WM_LBUTTONUP if id == LIST_ID => {
                    let result = DefSubclassProc(hwnd, message, wp, lp);
                    if IsWindowVisible(parent) != 0 {
                        SetFocus(state.edit.get());
                    }
                    return result;
                }
                WM_MOUSEWHEEL => {
                    scroll(parent, state, wp);
                    return 0;
                }
                WM_KEYDOWN | WM_SYSKEYDOWN if !state.composing.get() => {
                    let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                    let shift = GetKeyState(VK_SHIFT as i32) < 0;
                    let alt = GetKeyState(VK_MENU as i32) < 0;
                    let key = wp as u16;
                    if let Some(action) =
                        decode_action_key(key, ctrl, shift, alt, lp & (1 << 30) != 0)
                    {
                        match action {
                            ActionShortcut::Open if buttons::is_button(id) => {
                                button_click(parent, state, id, hwnd);
                            }
                            ActionShortcut::Open => {
                                queue_selected(parent, state, buttons::Kind::Open)
                            }
                            ActionShortcut::Elevated => {
                                queue_selected(parent, state, buttons::Kind::Elevated)
                            }
                            ActionShortcut::Repeat => (),
                        }
                        return 0;
                    }
                    let mut handled = true;
                    match key {
                        VK_ESCAPE => hide_window(parent, state, true),
                        VK_UP | VK_DOWN if !ctrl && !alt && !shift => {
                            navigate(parent, state, if key == VK_UP { -1 } else { 1 });
                        }
                        VK_TAB if !ctrl && !alt => {
                            buttons::tab(state, hwnd, shift);
                        }
                        _ => handled = false,
                    };
                    if handled {
                        return 0;
                    }
                }
                WM_CHAR if !state.composing.get() && matches!(wp, 9 | 10 | 13 | 27) => return 0,
                WM_SYSCHAR if !state.composing.get() => return 0,
                _ => (),
            }
        }
        DefSubclassProc(hwnd, message, wp, lp)
    }
}

fn copy_text(hwnd: HWND, text: &str) -> bool {
    let text = wide(text);
    unsafe {
        let buffer = GlobalAlloc(GMEM_MOVEABLE, text.len() * size_of::<u16>());
        if buffer.is_null() {
            return false;
        }
        let dest = GlobalLock(buffer).cast::<u16>();
        if dest.is_null() {
            GlobalFree(buffer);
            return false;
        }
        dest.copy_from_nonoverlapping(text.as_ptr(), text.len());
        GlobalUnlock(buffer);
        let success = if OpenClipboard(hwnd) != 0 {
            let success = EmptyClipboard() != 0
                && !SetClipboardData(CLIPBOARD_UNICODE_TEXT, buffer).is_null();
            CloseClipboard();
            success
        } else {
            false
        };
        if !success {
            GlobalFree(buffer);
        }
        success
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recreated_overlay_has_the_native_empty_edit_cue() {
        use windows_sys::Win32::UI::Controls::EM_GETCUEBANNER;

        struct Window(HWND);
        impl Drop for Window {
            fn drop(&mut self) {
                unsafe {
                    DestroyWindow(self.0);
                    UnregisterClassW(CLASS, GetModuleHandleW(null_mut()));
                }
            }
        }

        use i18n::{Language, with_language};
        let first = create(null_mut()).expect("create hidden search overlay");
        assert_ne!(unsafe { DestroyWindow(first) }, 0);
        let window = Window(with_language(Language::Ru, || {
            create(null_mut()).expect("reuse registered overlay class")
        }));
        unsafe {
            let edit = GetDlgItem(window.0, EDIT_ID as i32);
            assert!(!edit.is_null());
            assert_eq!(GetWindowTextLengthW(edit), 0);
            let cue = || {
                let mut value = [0u16; 128];
                assert_ne!(
                    SendMessageW(
                        edit,
                        EM_GETCUEBANNER,
                        value.as_mut_ptr() as usize,
                        value.len() as isize,
                    ),
                    0,
                );
                let end = value.iter().position(|&unit| unit == 0).unwrap();
                String::from_utf16(&value[..end]).unwrap()
            };
            assert_eq!(cue(), "Начните ввод");
            SetWindowTextW(edit, w!("unchanged"));
            with_language(Language::De, || refresh_language(window.0));
            assert_eq!(cue(), "Hier eingeben");
            let mut value = [0u16; 64];
            let length = GetWindowTextW(edit, value.as_mut_ptr(), value.len() as i32);
            assert_eq!(
                String::from_utf16(&value[..length as usize]).unwrap(),
                "unchanged"
            );
            let list = GetDlgItem(window.0, LIST_ID as i32);
            let copy = GetDlgItem(list, buttons::COPY as i32);
            let length = GetWindowTextW(copy, value.as_mut_ptr(), value.len() as i32);
            assert_eq!(
                String::from_utf16(&value[..length as usize]).unwrap(),
                "Ergebnis kopieren"
            );
            let length = GetWindowTextW(list, value.as_mut_ptr(), value.len() as i32);
            assert_eq!(
                String::from_utf16(&value[..length as usize]).unwrap(),
                "Suchergebnisse"
            );
        }
    }

    #[test]
    fn enter_requires_plain_or_ctrl_shift() {
        assert_eq!(
            decode_action_key(VK_RETURN, false, false, false, false),
            Some(ActionShortcut::Open),
        );
        assert_eq!(
            decode_action_key(VK_RETURN, true, true, false, false),
            Some(ActionShortcut::Elevated),
        );
        assert_eq!(
            decode_action_key(VK_RETURN, true, false, false, false),
            None
        );
        assert_eq!(
            decode_action_key(VK_RETURN, false, true, false, false),
            None
        );
        assert_eq!(
            decode_action_key(VK_RETURN, false, false, true, false),
            None
        );
        assert_eq!(decode_action_key(VK_RETURN, true, true, true, false), None);
    }

    #[test]
    fn digits_never_activate_a_result() {
        for key in VK_0..=VK_9 {
            assert_eq!(decode_action_key(key, false, false, true, false), None);
            assert_eq!(decode_action_key(key, true, true, true, false), None);
            assert_eq!(decode_action_key(key, false, false, false, false), None);
        }
    }

    #[test]
    fn enter_and_copy_button_copy_the_calculation() {
        let result = Results::Calculation("160".into());
        for kind in [buttons::Kind::Open, buttons::Kind::Copy] {
            assert!(
                matches!(action_for(&result, &[], 0, kind), Some(Action::Copy(value)) if value == "160")
            );
        }
        assert!(action_for(&result, &[], 1, buttons::Kind::Open).is_none());
        assert!(action_for(&result, &[], 0, buttons::Kind::Elevated).is_none());
    }

    #[test]
    fn held_launch_keys_are_consumed_without_new_action() {
        assert_eq!(
            decode_action_key(VK_RETURN, true, true, false, true),
            Some(ActionShortcut::Repeat),
        );
        assert_eq!(
            decode_action_key(VK_RETURN, false, false, false, true),
            Some(ActionShortcut::Repeat),
        );
    }

    #[test]
    fn viewport_keeps_selection_visible_when_populated_or_resized() {
        assert_eq!(visible_top(1, 0, 10), 0);
        assert_eq!(visible_top(0, 8, 10), 6);
        assert_eq!(visible_top(3, 4, 10), 3);
        assert_eq!(visible_top(9, 1, 2), 0);
        assert_eq!(visible_top(9, 9, 10), 7);
    }
}
