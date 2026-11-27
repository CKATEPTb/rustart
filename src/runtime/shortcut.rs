mod chord;

use super::ACTIVATE;
use crate::prefs::Hotkey;
use chord::{Chord, MODIFIERS, ordinary};
use std::cell::Cell;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, HWND, WAIT_OBJECT_0},
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{CreateEventW, GetCurrentProcessId, INFINITE, SetEvent},
    },
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

thread_local! {
    static CONTEXT: Cell<Option<HookContext>> = const { Cell::new(None) };
}

static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);
const MARKER_VK: u16 = 0xff;
const MARKER_TAG: usize = 0x5254_4d4b;

fn release_tag(token: usize) -> usize {
    token ^ 0x5254_5550
}

#[derive(Clone, Copy)]
struct HookContext {
    owner: HWND,
    token: usize,
    process: u32,
    mode: Mode,
    replaying: Option<u32>,
}

#[derive(Clone, Copy)]
enum Mode {
    Reserved(Press),
    Custom(Chord),
}

#[derive(Clone, Copy)]
enum Press {
    Idle,
    Forwarded,
    Captured,
}

impl Press {
    fn handle(self, down: bool, capture: impl FnOnce() -> bool) -> (Self, bool) {
        if !down {
            return (Self::Idle, matches!(self, Self::Captured));
        }
        match self {
            Self::Idle if capture() => (Self::Captured, true),
            Self::Idle | Self::Forwarded => (Self::Forwarded, false),
            Self::Captured => (Self::Captured, true),
        }
    }
}

fn win_only() -> bool {
    let down = |vk: u16| unsafe { GetAsyncKeyState(i32::from(vk)) < 0 };
    (down(VK_LWIN) || down(VK_RWIN)) && !down(VK_MENU) && !down(VK_CONTROL) && !down(VK_SHIFT)
}

fn activate_reserved(owner: HWND, token: usize) -> bool {
    if !win_only() {
        return false;
    }
    // Explorer must see that Win participated in a chord. Otherwise its
    // release can open Start after the overlay has taken focus.
    let marks = menu_markers();
    inject_all(&marks, send) && unsafe { PostMessageW(owner, ACTIVATE, token, 0) != 0 }
}

fn keyboard_input(key: KEYBDINPUT) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: key },
    }
}

fn menu_markers() -> [INPUT; 2] {
    [0, KEYEVENTF_KEYUP].map(|flags| {
        keyboard_input(KEYBDINPUT {
            wVk: MARKER_VK,
            dwFlags: flags,
            dwExtraInfo: MARKER_TAG,
            ..Default::default()
        })
    })
}

fn menu_safe_release(key: &KBDLLHOOKSTRUCT, token: usize) -> [INPUT; 3] {
    let [before, after] = menu_markers();
    let mut flags = KEYEVENTF_KEYUP;
    if key.flags & LLKHF_EXTENDED != 0 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    let (vk, scan) = if key.scanCode == 0 {
        (key.vkCode as u16, 0)
    } else {
        flags |= KEYEVENTF_SCANCODE;
        (0, key.scanCode as u16)
    };
    let release = keyboard_input(KEYBDINPUT {
        wVk: vk,
        wScan: scan,
        dwFlags: flags,
        dwExtraInfo: release_tag(token),
        ..Default::default()
    });
    [before, after, release]
}

fn inject_all(inputs: &[INPUT], inject: impl FnOnce(&[INPUT]) -> u32) -> bool {
    inject(inputs) == inputs.len() as u32
}

fn send(inputs: &[INPUT]) -> u32 {
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    }
}

unsafe extern "system" fn shortcut_hook(code: i32, wp: usize, lp: isize) -> isize {
    if code == HC_ACTION as i32
        && let Some(mut context) = CONTEXT.get()
    {
        let key = unsafe { &*(lp as *const KBDLLHOOKSTRUCT) };
        let down = matches!(wp as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        if key.flags & LLKHF_INJECTED != 0 {
            if key.vkCode == u32::from(MARKER_VK) && key.dwExtraInfo == MARKER_TAG {
                return unsafe { CallNextHookEx(null_mut(), code, wp, lp) };
            }
            if !down
                && context.replaying == Some(key.vkCode)
                && key.dwExtraInfo == release_tag(context.token)
            {
                context.replaying = None;
                CONTEXT.set(Some(context));
                let result = unsafe { CallNextHookEx(null_mut(), code, wp, lp) };
                unsafe { PostMessageW(context.owner, ACTIVATE, context.token, 0) };
                return result;
            }
        }
        match &mut context.mode {
            Mode::Reserved(press) if key.vkCode == u32::from(b'R') => {
                let (next, swallow) =
                    press.handle(down, || activate_reserved(context.owner, context.token));
                *press = next;
                CONTEXT.set(Some(context));
                if swallow {
                    return 1;
                }
            }
            Mode::Custom(chord) => {
                let trigger = chord.observe(
                    key.vkCode,
                    down,
                    |vk| unsafe { GetAsyncKeyState(vk as i32) < 0 },
                    || unsafe {
                        let mut foreground_process = 0;
                        GetWindowThreadProcessId(GetForegroundWindow(), &mut foreground_process);
                        foreground_process != context.process
                            && key.flags & LLKHF_LOWER_IL_INJECTED == 0
                    },
                );
                let suppress_menu = chord.blocks_system_menu();
                if trigger {
                    // Stage the neutral chord and replacement keyup in one
                    // batch. If UIPI denies it, let the real release through.
                    if suppress_menu {
                        context.replaying = Some(key.vkCode);
                        CONTEXT.set(Some(context));
                        if inject_all(&menu_safe_release(key, context.token), send) {
                            return 1;
                        }
                        context.replaying = None;
                    }
                    unsafe { PostMessageW(context.owner, ACTIVATE, context.token, 0) };
                }
                CONTEXT.set(Some(context));
            }
            Mode::Reserved(_) => {}
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, wp, lp) }
}

pub(super) struct StopEvent(usize);

impl StopEvent {
    fn new() -> Result<Self, String> {
        let handle = unsafe { CreateEventW(null_mut(), 1, 0, null_mut()) };
        if handle.is_null() {
            Err(std::io::Error::last_os_error().to_string())
        } else {
            Ok(Self(handle as usize))
        }
    }

    fn handle(&self) -> HANDLE {
        self.0 as HANDLE
    }
}

impl Drop for StopEvent {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle());
        }
    }
}

pub(super) enum Binding {
    Registered {
        owner: HWND,
        key: Hotkey,
        active: bool,
    },
    Hook {
        token: usize,
        stop: Arc<StopEvent>,
        worker: Option<JoinHandle<()>>,
    },
}

fn hotkey_id(key: Hotkey) -> i32 {
    // Separate IDs let the replacement be checked before releasing the old one.
    (key.vk * 16 + key.modifiers) as i32
}

fn register(owner: HWND, key: Hotkey) -> Result<(), String> {
    if unsafe { RegisterHotKey(owner, hotkey_id(key), key.modifiers | MOD_NOREPEAT, key.vk) } != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error().to_string();
    Err(crate::i18n::render(
        crate::i18n::Msg::TrayHotkeyConflict,
        &[("error", &error)],
    ))
}

impl Binding {
    pub(super) fn new(owner: HWND, key: Hotkey) -> Result<Self, String> {
        if key.modifiers & !MODIFIERS != 0
            || key.keys().any(|vk| !ordinary(vk))
            || (key.vk == 0 && (!key.extra_keys.is_empty() || key.modifiers == 0))
            || (!key.extra_keys.is_empty() && key.keys().count() < 2)
        {
            return Err(crate::i18n::tr(crate::i18n::Msg::HotkeyInvalid).into());
        }
        let reserved = key == Hotkey::default();
        if !reserved && !key.uses_custom() {
            register(owner, key)?;
            return Ok(Self::Registered {
                owner,
                key,
                active: true,
            });
        }
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::sync_channel(1);
        let stop = Arc::new(StopEvent::new()?);
        let worker_stop = Arc::clone(&stop);
        let owner = owner as usize;
        // A shell extension or modal dialog can block the UI thread. Keep the
        // low-level hook on its own sleeping message loop to meet Windows' timeout.
        let worker = thread::Builder::new()
            .name("rustart-shortcut".into())
            .spawn(move || unsafe {
                let mode = if reserved {
                    Mode::Reserved(if GetAsyncKeyState(i32::from(b'R')) < 0 {
                        Press::Forwarded
                    } else {
                        Press::Idle
                    })
                } else {
                    Mode::Custom(Chord::new(key))
                };
                CONTEXT.set(Some(HookContext {
                    owner: owner as HWND,
                    token,
                    process: GetCurrentProcessId(),
                    mode,
                    replaying: None,
                }));
                let mut msg = MSG::default();
                PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE);
                let hook = SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(shortcut_hook),
                    GetModuleHandleW(std::ptr::null()),
                    0,
                );
                if hook.is_null() {
                    let _ = tx.send(Err(std::io::Error::last_os_error().to_string()));
                    return;
                }
                if tx.send(Ok(())).is_ok() {
                    let handle = worker_stop.handle();
                    while MsgWaitForMultipleObjectsEx(
                        1,
                        &handle,
                        INFINITE,
                        QS_ALLINPUT,
                        MWMO_INPUTAVAILABLE,
                    ) == WAIT_OBJECT_0 + 1
                    {
                        if PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                            if msg.message == WM_QUIT {
                                break;
                            }
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                }
                UnhookWindowsHookEx(hook);
            })
            .map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())??;
        Ok(Self::Hook {
            token,
            stop,
            worker: Some(worker),
        })
    }

    // Let the focused launcher accept its own text and editing shortcuts, even
    // if the user chose a letter, Enter or Ctrl+Shift+Enter as the global key.
    pub(super) fn text_focus(&mut self, editing: bool) -> Result<(), String> {
        if let Self::Registered { owner, key, active } = self
            && key.modifiers & (MOD_WIN | MOD_ALT) == 0
            && *active == editing
        {
            if editing {
                if unsafe { UnregisterHotKey(*owner, hotkey_id(*key)) } == 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
            } else {
                register(*owner, *key)?;
            }
            *active = !editing;
        }
        Ok(())
    }

    pub(super) fn accepts_hotkey(&self, id: usize, input: isize) -> bool {
        let Self::Registered {
            key, active: true, ..
        } = self
        else {
            return false;
        };
        // Windows can deliver a message queued before suspension or before a
        // replacement was committed. A notification only applies to its binding.
        id == hotkey_id(*key) as usize
            && (input as u32 & (MOD_WIN | MOD_ALT | MOD_CONTROL | MOD_SHIFT)) == key.modifiers
            && (input as u32 >> 16) == key.vk
    }

    pub(super) fn accepts_activation(&self, id: usize) -> bool {
        matches!(self, Self::Hook { token, .. } if *token == id)
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        unsafe {
            match self {
                Self::Registered { owner, key, active } => {
                    if *active {
                        UnregisterHotKey(*owner, hotkey_id(*key));
                    }
                }
                Self::Hook { stop, worker, .. } => {
                    if SetEvent(stop.handle()) != 0
                        && let Some(worker) = worker.take()
                    {
                        let _ = worker.join();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_suppression_needs_a_complete_batch_ending_with_the_original_keyup() {
        let original = KBDLLHOOKSTRUCT {
            vkCode: u32::from(VK_RMENU),
            scanCode: 0x38,
            flags: LLKHF_UP | LLKHF_EXTENDED,
            ..Default::default()
        };
        let batch = menu_safe_release(&original, 42);
        let [down, up, release] = batch.map(|input| unsafe { input.Anonymous.ki });
        assert_eq!((down.wVk, down.dwFlags), (MARKER_VK, 0));
        assert_eq!((up.wVk, up.dwFlags), (MARKER_VK, KEYEVENTF_KEYUP));
        assert_eq!(release.wScan, original.scanCode as u16);
        assert_eq!(release.dwExtraInfo, release_tag(42));
        assert_eq!(
            release.dwFlags,
            KEYEVENTF_KEYUP | KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY
        );
        for accepted in 0..3 {
            assert!(!inject_all(&batch, |_| accepted));
        }
        assert!(inject_all(&batch, |_| 3));
    }

    #[test]
    fn only_the_active_matching_registration_accepts_a_notification() {
        use std::mem::ManuallyDrop;
        let key = Hotkey::new(0, u32::from(b'Q'));
        // This test exercises the state predicate without touching Win32's
        // desktop-wide shortcut table.
        let mut binding = ManuallyDrop::new(Binding::Registered {
            owner: null_mut(),
            key,
            active: true,
        });
        let id = hotkey_id(key) as usize;
        let input = (key.vk << 16) as isize;
        assert!(binding.accepts_hotkey(id, input));
        assert!(!binding.accepts_hotkey(id + 1, input));
        assert!(!binding.accepts_hotkey(id, (u32::from(b'R') << 16) as isize));
        assert!(!binding.accepts_hotkey(id, input | MOD_CONTROL as isize));
        if let Binding::Registered { active, .. } = &mut *binding {
            *active = false;
        }
        assert!(!binding.accepts_hotkey(id, input));
    }

    #[test]
    fn a_forwarded_press_stays_forwarded_through_repeats_and_release() {
        let (press, swallow) = Press::Idle.handle(true, || false);
        assert!(!swallow);

        let (press, swallow) = press.handle(true, || panic!("repeat must not attempt activation"));
        assert!(!swallow);
        let (press, swallow) = press.handle(false, || unreachable!());
        assert!(!swallow);
        assert!(matches!(press, Press::Idle));
    }

    #[test]
    fn hook_activation_matches_only_the_current_binding_token() {
        let current = Binding::Hook {
            token: 42,
            stop: Arc::new(StopEvent::new().unwrap()),
            worker: None,
        };
        assert!(current.accepts_activation(42));
        assert!(!current.accepts_activation(41));
        assert!(!current.accepts_activation(0));
        assert!(!current.accepts_hotkey(42, 0));
    }

    #[test]
    fn an_activated_press_consumes_repeats_and_release() {
        let (press, swallow) = Press::Idle.handle(true, || true);
        assert!(swallow);
        let (press, swallow) = press.handle(true, || panic!("repeat must not attempt activation"));
        assert!(swallow);
        let (press, swallow) = press.handle(false, || unreachable!());
        assert!(swallow);
        assert!(matches!(press, Press::Idle));
    }
}
