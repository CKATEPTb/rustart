mod catalog_watch;
mod instance;
mod shortcut;
pub(crate) use instance::{
    client, elevated_background, elevated_startup, installation_owner, status,
};

use crate::{
    catalog,
    i18n::{Msg, tr},
    overlay,
    prefs::{self, Hotkey, Prefs},
    tray,
};
use shortcut::Binding;
use std::{
    cell::RefCell,
    ptr::{null, null_mut},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use windows_sys::{
    Win32::{
        Foundation::{HWND, WAIT_FAILED, WAIT_OBJECT_0},
        System::{
            Com::{
                COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoFreeUnusedLibrariesEx,
                CoInitializeEx, CoUninitialize,
            },
            LibraryLoader::GetModuleHandleW,
            Memory::{HeapOptimizeResources, HeapSetInformation},
            SystemServices::{
                HEAP_OPTIMIZE_RESOURCES_CURRENT_VERSION, HEAP_OPTIMIZE_RESOURCES_INFORMATION,
            },
            Threading::INFINITE,
        },
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

const OWNER_CLASS: PCWSTR = w!("CKATEPTb.Rustart.Owner.1");
pub(crate) const SHOW: u32 = WM_APP + 10;
pub(crate) const HIDE: u32 = WM_APP + 11;
pub(crate) const QUIT: u32 = WM_APP + 12;
pub(crate) const REFRESH: u32 = WM_APP + 13;
pub(crate) const PING: u32 = WM_APP + 14;
const ACTIVATE: u32 = WM_APP + 15;
const STOP: u32 = WM_APP + 16;
const INDEX_READY: u32 = WM_APP + 17;
const START_ERROR: u32 = WM_APP + 18;
const PACKAGE_CHANGED: u32 = WM_APP + 19;
const INDEX_OPEN: u32 = WM_APP + 20;
const INDEX_TIMER: usize = 20;
const INDEX_DEBOUNCE: Duration = Duration::from_millis(250);

struct App {
    owner: HWND,
    overlay: HWND,
    prefs: Rc<Prefs>,
    shortcut: Option<Binding>,
    fetching: Option<mpsc::Receiver<IndexUpdate>>,
    pending_index: PendingIndex,
    debounce: Option<Instant>,
    watch: Option<catalog_watch::Watch>,
    preparing_watch: bool,
    package_signal: Arc<AtomicBool>,
    warning: Option<String>,
}
thread_local! { static APP: RefCell<Option<App>> = const { RefCell::new(None) }; }

struct IndexUpdate {
    apps: Result<Vec<catalog::App>, String>,
    steam: catalog_watch::SteamFolders,
}

struct PendingIndex {
    dirty: bool,
    explicit: bool,
}

impl PendingIndex {
    fn invalidate(&mut self, explicit: bool) {
        self.dirty = true;
        self.explicit |= explicit;
    }

    fn begin(&mut self) {
        self.dirty = false;
        self.explicit = false;
    }

    fn completed(&mut self, success: bool, visible: bool) -> bool {
        let scan_again = self.dirty && (visible || self.explicit);
        self.dirty |= !success;
        scan_again
    }
}

pub(crate) fn error_box(owner: HWND, text: &str) {
    let text: Vec<_> = text.encode_utf16().chain([0]).collect();
    unsafe {
        MessageBoxW(owner, text.as_ptr(), w!("Rustart"), MB_OK | MB_ICONWARNING);
    }
}

fn snapshot() -> Option<(HWND, HWND, Rc<Prefs>)> {
    APP.with_borrow(|app| {
        app.as_ref()
            .map(|a| (a.owner, a.overlay, Rc::clone(&a.prefs)))
    })
}

pub(crate) fn update_hotkey(key: Hotkey) -> Result<(), String> {
    let (owner, _, prev) = snapshot().ok_or_else(|| tr(Msg::InstanceStopping))?;
    if prev.hotkey == key && APP.with_borrow(|s| s.as_ref().is_some_and(|s| s.shortcut.is_some())) {
        return Ok(());
    }
    let mut new_binding = Binding::new(owner, key)?;
    let mut foreground_pid = 0;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut foreground_pid) };
    new_binding.text_focus(foreground_pid == std::process::id())?;
    let mut next = (*prev).clone();
    next.hotkey = key;
    prefs::save(&next)?;
    let old = APP.with_borrow_mut(|state| {
        let state = state.as_mut()?;
        state.prefs = Rc::new(next);
        state.shortcut.replace(new_binding)
    });
    drop(old);
    Ok(())
}

pub(crate) fn update_language(language: Option<crate::i18n::Language>) -> Result<(), String> {
    let (_, overlay, prev) = snapshot().ok_or_else(|| tr(Msg::InstanceStopping))?;
    if prev.language != language {
        let mut next = (*prev).clone();
        next.language = language;
        prefs::save(&next)?;
        APP.with_borrow_mut(|app| {
            if let Some(state) = app {
                state.prefs = Rc::new(next);
            }
        });
    }
    crate::i18n::set_language(language);
    if !overlay.is_null() {
        overlay::refresh_language(overlay);
    }
    Ok(())
}

fn prepare_watch() {
    let target = APP.with_borrow_mut(|s| {
        let s = s.as_mut()?;
        if s.preparing_watch
            || s.watch
                .as_ref()
                .is_some_and(catalog_watch::Watch::desktop_complete)
        {
            return None;
        }
        s.preparing_watch = true;
        Some((s.owner, Arc::clone(&s.package_signal)))
    });
    if let Some((owner, signal)) = target {
        let watch = catalog_watch::Watch::new(owner, signal);
        let previous = APP.with_borrow_mut(|s| {
            let s = s.as_mut()?;
            s.preparing_watch = false;
            s.pending_index.invalidate(false);
            s.watch.replace(watch)
        });
        drop(previous);
    }
}

fn cancel_debounce(owner: HWND) {
    APP.with_borrow_mut(|s| {
        if let Some(state) = s {
            state.debounce = None;
        }
    });
    unsafe { KillTimer(owner, INDEX_TIMER) };
}

fn invalidate_index(owner: HWND, overlay: HWND) {
    let begun = APP.with_borrow_mut(|state| {
        let Some(state) = state.as_mut() else {
            return false;
        };
        state.pending_index.invalidate(false);
        state.watch.is_some()
    });
    if begun && overlay::showing_app_search(overlay) {
        let deadline = Instant::now() + INDEX_DEBOUNCE;
        if unsafe { SetTimer(owner, INDEX_TIMER, INDEX_DEBOUNCE.as_millis() as u32, None) } != 0 {
            APP.with_borrow_mut(|s| {
                if let Some(state) = s {
                    state.debounce = Some(deadline);
                }
            });
        } else {
            cancel_debounce(owner);
            request_index();
        }
    }
}

fn request_index() {
    let target = APP.with_borrow(|s| {
        let s = s.as_ref()?;
        (s.pending_index.dirty
            && !s.preparing_watch
            && s.fetching.is_none()
            && (s.debounce.is_none() || s.pending_index.explicit))
            .then_some(s.owner as usize)
    });
    let Some(owner) = target else {
        return;
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("rustart-index".into())
        .spawn(move || {
            // Subscribe before discovery so changes during the scan remain
            // signalled even before the UI starts waiting on these folders.
            let mut folders = catalog::steam_library_folders();
            if let Some(primary) = folders.first()
                && !primary.join("libraryfolders.vdf").is_file()
                && let Some(root) = primary.parent()
            {
                folders.push(root.join("config"));
            }
            let steam = catalog_watch::SteamFolders::open(&folders);
            let apps = catalog::discover_isolated();
            if tx.send(IndexUpdate { apps, steam }).is_ok() {
                unsafe {
                    PostMessageW(owner as HWND, INDEX_READY, 0, 0);
                }
            }
        });
    match worker {
        Ok(_) => APP.with_borrow_mut(|s| {
            if let Some(s) = s {
                s.fetching = Some(rx);
                s.pending_index.begin();
            }
        }),
        Err(e) => crate::diagnostic(format_args!("Не удалось начать обновление списка. {e}")),
    }
}

fn receive_index(overlay: HWND) {
    let showing_search = overlay::showing_app_search(overlay);
    let result = APP.with_borrow_mut(|app| {
        let s = app.as_mut()?;
        let result = match s.fetching.as_ref()?.try_recv() {
            Ok(update) => {
                if let Some(watch) = &mut s.watch {
                    watch.steam = update.steam;
                }
                update.apps
            }
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err("Индексирование было прервано.".into()),
        };
        s.fetching = None;
        let again = s.pending_index.completed(result.is_ok(), showing_search);
        Some((result, again))
    });
    if let Some((result, again)) = result {
        match result {
            Ok(apps) => overlay::set_catalog(overlay, apps),
            Err(e) => crate::diagnostic(format_args!("Не удалось обновить список. {e}")),
        }
        if again {
            request_index();
        }
    }
}

fn open_index(owner: HWND, overlay: HWND, explicit: bool) {
    if !explicit && !overlay::showing_app_search(overlay) {
        return;
    }
    prepare_watch();
    APP.with_borrow_mut(|s| {
        if let Some(state) = s
            && (explicit
                || state
                    .watch
                    .as_ref()
                    .is_some_and(|watch| !watch.steam.complete))
        {
            state.pending_index.invalidate(explicit);
        }
    });
    cancel_debounce(owner);
    request_index();
}

fn queue_index_open(owner: HWND, overlay: HWND, explicit: bool) {
    if unsafe { PostMessageW(owner, INDEX_OPEN, usize::from(explicit), 0) } == 0 {
        open_index(owner, overlay, explicit);
    }
}

pub(crate) fn search_started(overlay: HWND) {
    queue_index_open(unsafe { GetWindow(overlay, GW_OWNER) }, overlay, false);
}

pub(crate) fn overlay_idle() {
    // The view has dropped its UI caches. Release unused COM servers and
    // let Windows decommit free heap pages while the user is away.
    let options = HEAP_OPTIMIZE_RESOURCES_INFORMATION {
        Version: HEAP_OPTIMIZE_RESOURCES_CURRENT_VERSION,
        Flags: 0,
    };
    unsafe {
        CoFreeUnusedLibrariesEx(0, 0);
        HeapSetInformation(
            null_mut(),
            HeapOptimizeResources,
            (&raw const options).cast(),
            size_of_val(&options),
        );
    }
}

fn ensure_overlay(owner: HWND, overlay: HWND) -> Result<HWND, String> {
    if !overlay.is_null() {
        return Ok(overlay);
    }
    let hwnd = overlay::create(owner)?;
    APP.with_borrow_mut(|app| {
        if let Some(state) = app {
            state.overlay = hwnd;
        }
    });
    Ok(hwnd)
}

unsafe extern "system" fn wndproc(owner: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    if msg == WM_DESTROY {
        unsafe {
            PostQuitMessage(0);
        }
        return 0;
    }
    if let Some((_, overlay, prefs)) = snapshot() {
        match msg {
            WM_ACTIVATEAPP => {
                let result = APP.with_borrow_mut(|state| {
                    state
                        .as_mut()?
                        .shortcut
                        .as_mut()
                        .map(|binding| binding.text_focus(wp != 0))
                });
                if let Some(Err(error)) = result {
                    crate::diagnostic(error);
                }
                return 0;
            }
            PING => {
                return 1 + isize::from(
                    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() } != 0,
                );
            }
            WM_HOTKEY
                if !APP.with_borrow(|state| {
                    state
                        .as_ref()
                        .and_then(|app| app.shortcut.as_ref())
                        .is_some_and(|key| key.accepts_hotkey(wp, lp))
                }) =>
            {
                return 0;
            }
            ACTIVATE
                if !APP.with_borrow(|state| {
                    state
                        .as_ref()
                        .and_then(|app| app.shortcut.as_ref())
                        .is_some_and(|key| key.accepts_activation(wp))
                }) =>
            {
                return 0;
            }
            SHOW | ACTIVATE | WM_HOTKEY => {
                if tray::editing_hotkey()
                    || unsafe {
                        windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(owner)
                    } == 0
                {
                    let dialog = unsafe { GetLastActivePopup(owner) };
                    if dialog != owner {
                        unsafe {
                            SetForegroundWindow(dialog);
                        }
                    }
                    return 3;
                }
                let overlay = match ensure_overlay(owner, overlay) {
                    Ok(hwnd) => hwnd,
                    Err(error) => {
                        error_box(owner, &error);
                        return 4;
                    }
                };
                overlay::show(overlay);
                return if unsafe { GetForegroundWindow() } == overlay {
                    1
                } else {
                    4
                };
            }
            HIDE => {
                if !overlay.is_null() {
                    overlay::hide(overlay);
                }
                return 1;
            }
            INDEX_OPEN => {
                open_index(owner, overlay, wp != 0);
                return 0;
            }
            REFRESH => {
                match ensure_overlay(owner, overlay) {
                    Ok(hwnd) => queue_index_open(owner, hwnd, true),
                    Err(error) => {
                        crate::diagnostic(error);
                        return 0;
                    }
                }
                return 1;
            }
            PACKAGE_CHANGED => {
                APP.with_borrow(|s| {
                    if let Some(state) = s.as_ref() {
                        state.package_signal.store(false, Ordering::Release);
                    }
                });
                invalidate_index(owner, overlay);
                return 0;
            }
            WM_TIMER if wp == INDEX_TIMER => {
                let due = APP
                    .with_borrow(|s| s.as_ref().and_then(|state| state.debounce))
                    .is_none_or(|deadline| Instant::now() >= deadline);
                if due {
                    cancel_debounce(owner);
                    if overlay::showing_app_search(overlay) {
                        request_index();
                    }
                }
                return 0;
            }
            INDEX_READY => {
                receive_index(overlay);
                return 0;
            }
            QUIT | WM_CLOSE => {
                unsafe {
                    PostMessageW(owner, STOP, 0, 0);
                }
                return 1;
            }
            STOP => {
                tray::cancel_hotkey_capture();
                unsafe {
                    PostQuitMessage(0);
                }
                return 0;
            }
            START_ERROR => {
                let warning = APP.with_borrow_mut(|s| s.as_mut().and_then(|s| s.warning.take()));
                if let Some(message) = warning {
                    error_box(owner, &message);
                }
                return 0;
            }
            _ => {}
        }
        // Tray actions can enter nested Win32 message loops.
        // No borrow of thread-local state survives across application callbacks.
        if let Some(result) = tray::handle(owner, msg, wp, lp, &prefs) {
            return result;
        }
    }
    unsafe { DefWindowProcW(owner, msg, wp, lp) }
}

pub(crate) fn serve() -> Result<(), String> {
    let Some(_guard) = instance::elect()? else {
        return Ok(());
    };
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let hr = unsafe {
        CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    if hr < 0 {
        return Err(format!("COM: {hr:#x}"));
    }
    let result = run_loop();
    unsafe {
        CoUninitialize();
    }
    result
}

fn run_loop() -> Result<(), String> {
    let instance = unsafe { GetModuleHandleW(null()) };
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        lpszClassName: OWNER_CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let owner = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            OWNER_CLASS,
            w!("Rustart"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    if owner.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    struct Owner(HWND);
    impl Drop for Owner {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.0);
            }
        }
    }
    let _window = Owner(owner);
    let (prefs, mut warning) = match prefs::load() {
        Ok(prefs) => (prefs, None),
        Err(err) => (Prefs::default(), Some(err)),
    };
    crate::i18n::set_language(prefs.language);
    let binding = match Binding::new(owner, prefs.hotkey) {
        Ok(binding) => Some(binding),
        Err(err) => {
            warning = Some(format!(
                "{err}\n{}",
                crate::i18n::tr(crate::i18n::Msg::HotkeySetupHelp)
            ));
            None
        }
    };
    // An ordinary CLI may signal an elevated resident. These five messages
    // have no payload and cannot select or execute an application.
    for action in [PING, SHOW, HIDE, QUIT, REFRESH] {
        if unsafe { ChangeWindowMessageFilterEx(owner, action, MSGFLT_ALLOW, null_mut()) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    tray::install(owner)?;
    if warning.is_some() {
        unsafe {
            PostMessageW(owner, START_ERROR, 0, 0);
        }
    }
    APP.set(Some(App {
        owner,
        overlay: null_mut(),
        prefs: Rc::new(prefs),
        shortcut: binding,
        fetching: None,
        pending_index: PendingIndex {
            dirty: true,
            explicit: false,
        },
        debounce: None,
        watch: None,
        preparing_watch: false,
        package_signal: Arc::new(AtomicBool::new(false)),
        warning,
    }));
    // Set the discoverable title only after messages can be handled, including
    // clients at normal integrity contacting an elevated resident.
    if unsafe { SetWindowTextW(owner, instance::READY_TITLE) } == 0 {
        let err = std::io::Error::last_os_error().to_string();
        drop(APP.take());
        tray::remove(owner);
        return Err(err);
    }
    let mut msg = MSG::default();
    let mut handles = Vec::new();
    let result = loop {
        handles.clear();
        APP.with_borrow(|s| {
            if let Some(watch) = s.as_ref().and_then(|state| state.watch.as_ref()) {
                watch.handles(&mut handles);
            }
        });
        let count = handles.len() as u32;
        let next = if count == 0 {
            unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) }
        } else {
            let wake = unsafe {
                MsgWaitForMultipleObjectsEx(
                    count,
                    handles.as_ptr(),
                    INFINITE,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                )
            };
            if wake == WAIT_FAILED || (WAIT_OBJECT_0..WAIT_OBJECT_0 + count).contains(&wake) {
                APP.with_borrow_mut(|s| {
                    if let Some(watch) = s.as_mut().and_then(|state| state.watch.as_mut()) {
                        if wake == WAIT_FAILED {
                            watch.stop_folders();
                        } else {
                            watch.rearm(handles[(wake - WAIT_OBJECT_0) as usize]);
                        }
                    }
                });
                if let Some((_, overlay, _)) = snapshot() {
                    invalidate_index(owner, overlay);
                }
            }
            if unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } == 0 {
                continue;
            }
            i32::from(msg.message != WM_QUIT)
        };
        match next {
            0 => break Ok(()),
            -1 => break Err(std::io::Error::last_os_error().to_string()),
            _ => unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    };
    drop(APP.take());
    tray::remove(owner);
    result
}

#[cfg(test)]
mod tests {
    use super::PendingIndex;

    #[test]
    fn scan_completion_keeps_changes_that_arrived_during_work() {
        let mut index = PendingIndex {
            dirty: true,
            explicit: false,
        };
        index.begin();
        assert!(!index.completed(true, true));
        index.invalidate(false);
        assert!(index.completed(true, true));
        index.begin();
        index.invalidate(false);
        assert!(!index.completed(true, false));
        assert!(index.dirty);
        index.begin();
        index.invalidate(true);
        assert!(index.completed(true, false));
        index.begin();
        assert!(!index.completed(false, true));
        assert!(index.dirty);
    }
}
