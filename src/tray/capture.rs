use std::{
    cell::Cell,
    io,
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

use crate::prefs::{Hotkey, KeySet};

pub(super) const KEY_EVENT: u32 = WM_APP + 44;
const MODIFIERS: [(u32, &str); 4] = [
    (MOD_WIN, "Win"),
    (MOD_CONTROL, "Ctrl"),
    (MOD_ALT, "Alt"),
    (MOD_SHIFT, "Shift"),
];

thread_local! {
    static TARGET: Cell<(HWND, usize)> = const { Cell::new((null_mut(), 0)) };
    static LOST: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn active() -> bool {
    !TARGET.get().0.is_null()
}

fn key_label(vk: u32) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5a => (vk as u8 as char).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        // Map scan codes for punctuation and the extended non-letter keys.
        _ => {
            let scan = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX) };
            let extended = if scan & 0xff00 != 0 { 1 << 24 } else { 0 };
            let mut name = [0u16; 40];
            let count = unsafe {
                GetKeyNameTextW(
                    ((scan & 0xff) << 16 | extended) as i32,
                    name.as_mut_ptr(),
                    name.len() as i32,
                )
            };
            if count > 0 {
                String::from_utf16_lossy(&name[..count as usize])
            } else {
                format!("VK {:02X}", vk)
            }
        }
    }
}

pub(super) fn hotkey_label(hotkey: Hotkey) -> String {
    let mut keys = MODIFIERS
        .iter()
        .filter_map(|&(flag, name)| (hotkey.modifiers & flag != 0).then_some(name.to_owned()))
        .collect::<Vec<_>>();
    keys.extend(hotkey.keys().map(key_label));
    keys.join(" + ")
}

pub(super) struct Capture {
    hook: HHOOK,
    pub generation: usize,
    pub keys: Keys,
}

impl Capture {
    pub(super) fn start(hwnd: HWND) -> io::Result<Self> {
        let hook =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), GetModuleHandleW(null()), 0) };
        if hook.is_null() {
            return Err(io::Error::last_os_error());
        }
        let generation = TARGET.with(|target| {
            let generation = target.get().1.wrapping_add(1);
            target.set((hwnd, generation));
            generation
        });
        LOST.set(false);
        let held = [
            VK_LCONTROL,
            VK_RCONTROL,
            VK_LMENU,
            VK_RMENU,
            VK_LSHIFT,
            VK_RSHIFT,
            VK_LWIN,
            VK_RWIN,
        ]
        .into_iter()
        .filter(|&key| unsafe { GetAsyncKeyState(i32::from(key)) } < 0)
        .collect();
        let ignored = [VK_RETURN, VK_SPACE]
            .into_iter()
            .filter(|&key| unsafe { GetAsyncKeyState(i32::from(key)) } < 0)
            .collect();
        Ok(Self {
            hook,
            generation,
            keys: Keys {
                held,
                ignored,
                ..Keys::default()
            },
        })
    }

    pub(super) fn take_delivery_failure() -> bool {
        LOST.replace(false)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // A late queued message can never mutate the next capture.
        if TARGET.get().1 == self.generation {
            TARGET.set((null_mut(), self.generation));
        }
        unsafe { UnhookWindowsHookEx(self.hook) };
    }
}

unsafe extern "system" fn on_key(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let (hwnd, generation) = TARGET.get();
        if code == HC_ACTION as i32 && !hwnd.is_null() && GetForegroundWindow() == hwnd {
            let key = (*(lp as *const KBDLLHOOKSTRUCT)).vkCode;
            let down = matches!(wp as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            if (1..=254).contains(&key)
                && !matches!(key as u16, VK_PACKET | VK_PROCESSKEY)
                && matches!(
                    wp as u32,
                    WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP
                )
            {
                if PostMessageW(
                    hwnd,
                    KEY_EVENT,
                    generation,
                    key as isize | (isize::from(down) << 16),
                ) == 0
                {
                    LOST.set(true);
                }
                return 1;
            }
        }
        CallNextHookEx(null_mut(), code, wp, lp)
    }
}

#[derive(Default)]
pub(super) struct Keys {
    held: Vec<u16>,
    ignored: Vec<u16>,
    chord: Option<Hotkey>,
    cancelling: bool,
}

#[derive(PartialEq, Eq, Debug)]
pub(super) enum Event {
    Unchanged,
    Changed,
    Cancel,
    Complete(Hotkey),
    Retry,
}

fn modifier(key: u16) -> u32 {
    match key {
        VK_CONTROL | VK_LCONTROL | VK_RCONTROL => MOD_CONTROL,
        VK_MENU | VK_LMENU | VK_RMENU => MOD_ALT,
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT => MOD_SHIFT,
        VK_LWIN | VK_RWIN => MOD_WIN,
        _ => 0,
    }
}

impl Keys {
    fn modifiers(&self) -> u32 {
        self.held.iter().fold(0, |mods, &key| mods | modifier(key))
    }

    pub(super) fn preview(&self) -> String {
        if let Some(chord) = self.chord {
            return hotkey_label(chord);
        }
        hotkey_label(Hotkey::new(self.modifiers(), 0))
    }

    fn remember_simultaneous_keys(&mut self) {
        let mut names = KeySet::default();
        for &key in &self.held {
            if modifier(key) == 0 {
                names.insert(u32::from(key));
            }
        }
        let vk = names.iter().next().unwrap_or_default();
        names.remove(vk);
        let candidate = Hotkey {
            modifiers: self.modifiers(),
            vk,
            extra_keys: names,
        };
        let score = |chord: Hotkey| {
            let ordinary = chord.keys().count();
            (
                ordinary != 0,
                ordinary + chord.modifiers.count_ones() as usize,
                ordinary,
            )
        };
        // Keep the largest chord which was physically held together. Rolling
        // from A+B to B+C does not fabricate a chord A+B+C.
        if self
            .chord
            .is_none_or(|previous| score(candidate) > score(previous))
        {
            self.chord = Some(candidate);
        }
    }

    pub(super) fn handle(&mut self, key: u16, down: bool) -> Event {
        if key == VK_ESCAPE {
            self.cancelling = down;
            return if down {
                Event::Unchanged
            } else {
                Event::Cancel
            };
        }
        if self.cancelling {
            return Event::Unchanged;
        }
        if let Some(index) = self.ignored.iter().position(|&ignored| ignored == key) {
            if !down {
                self.ignored.swap_remove(index);
            }
            return Event::Unchanged;
        }
        if down {
            if self.held.contains(&key) {
                return Event::Unchanged;
            }
            self.held.push(key);
            self.remember_simultaneous_keys();
        } else if let Some(pos) = self.held.iter().position(|&held| key == held) {
            self.held.swap_remove(pos);
            if self.held.is_empty()
                && let Some(chord) = self.chord.take()
            {
                return Event::Complete(chord);
            }
        } else {
            // In particular, Enter/Space which started recording may be
            // released after the temporary hook has already installed.
            return Event::Unchanged;
        }
        Event::Changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_chord_is_delivered_after_last_release() {
        let mut keys = Keys::default();
        assert_eq!(keys.handle(VK_RETURN, false), Event::Unchanged);
        assert_eq!(keys.handle(VK_LWIN, true), Event::Changed);
        assert_eq!(keys.handle(b'K' as u16, true), Event::Changed);
        assert_eq!(keys.handle(b'K' as u16, true), Event::Unchanged);
        assert_eq!(keys.handle(b'K' as u16, false), Event::Changed);
        assert_eq!(
            keys.handle(VK_LWIN, false),
            Event::Complete(Hotkey::new(MOD_WIN, b'K'.into()))
        );
    }

    #[test]
    fn a_bare_key_is_allowed_and_escape_cancels_recording() {
        let mut keys = Keys::default();
        keys.handle(VK_F7, true);
        assert_eq!(
            keys.handle(VK_F7, false),
            Event::Complete(Hotkey::new(0, u32::from(VK_F7)))
        );
        assert_eq!(keys.handle(VK_ESCAPE, true), Event::Unchanged);
        assert_eq!(keys.handle(VK_F8, true), Event::Unchanged);
        assert_eq!(keys.handle(VK_F8, false), Event::Unchanged);
        assert_eq!(keys.handle(VK_ESCAPE, false), Event::Cancel);
    }

    #[test]
    fn the_key_that_opened_recording_must_be_released_before_it_can_be_assigned() {
        let mut keys = Keys {
            ignored: vec![VK_RETURN],
            ..Keys::default()
        };
        assert_eq!(keys.handle(VK_RETURN, true), Event::Unchanged);
        assert_eq!(keys.handle(VK_RETURN, false), Event::Unchanged);
        assert_eq!(keys.handle(VK_RETURN, true), Event::Changed);
        assert_eq!(
            keys.handle(VK_RETURN, false),
            Event::Complete(Hotkey::new(0, u32::from(VK_RETURN)))
        );
    }

    #[test]
    fn records_simultaneous_keys_without_combining_rolling_keys() {
        let mut keys = Keys::default();
        for key in [VK_LCONTROL, u16::from(b'B'), u16::from(b'A')] {
            assert_eq!(keys.handle(key, true), Event::Changed);
        }
        keys.handle(u16::from(b'A'), false);
        keys.handle(u16::from(b'C'), true);
        keys.handle(u16::from(b'C'), false);
        keys.handle(u16::from(b'B'), false);
        let Event::Complete(chord) = keys.handle(VK_LCONTROL, false) else {
            panic!("missing chord")
        };
        assert_eq!(chord.modifiers, MOD_CONTROL);
        assert_eq!(
            chord.keys().collect::<Vec<_>>(),
            [u32::from(b'A'), u32::from(b'B')]
        );
    }

    #[test]
    fn releases_the_last_modifier_to_commit_a_modifier_only_chord() {
        let mut keys = Keys::default();
        assert_eq!(keys.handle(VK_LCONTROL, true), Event::Changed);
        assert_eq!(keys.handle(VK_RSHIFT, true), Event::Changed);
        assert_eq!(keys.handle(VK_LCONTROL, false), Event::Changed);
        assert_eq!(
            keys.handle(VK_RSHIFT, false),
            Event::Complete(Hotkey::new(MOD_CONTROL | MOD_SHIFT, 0))
        );
    }
}
