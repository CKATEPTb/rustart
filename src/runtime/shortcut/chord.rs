use crate::prefs::{Hotkey, KeySet};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

pub(super) const MODIFIERS: u32 = MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN;

fn modifier(vk: u32) -> u32 {
    match vk as u16 {
        VK_LMENU | VK_RMENU | VK_MENU => MOD_ALT,
        VK_LCONTROL | VK_RCONTROL | VK_CONTROL => MOD_CONTROL,
        VK_LSHIFT | VK_RSHIFT | VK_SHIFT => MOD_SHIFT,
        VK_LWIN | VK_RWIN => MOD_WIN,
        _ => 0,
    }
}

pub(super) fn ordinary(vk: u32) -> bool {
    (1..=254).contains(&vk) && modifier(vk) == 0
}

#[derive(Clone, Copy)]
pub(super) struct Chord {
    key: Hotkey,
    phase: Phase,
    foreign_held: KeySet,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Tracking,
    Complete,
    Interrupted,
}

impl Phase {
    fn advance(
        &mut self,
        clear_before: bool,
        clear_after: bool,
        complete: bool,
        interrupted: bool,
    ) -> bool {
        if clear_before {
            *self = Self::Idle;
        }
        if clear_after {
            let activate = !interrupted && *self == Self::Complete;
            *self = Self::Idle;
            return activate;
        }
        if interrupted || (*self == Self::Idle && !clear_before) {
            *self = Self::Interrupted;
        } else if complete && *self != Self::Interrupted {
            *self = Self::Complete;
        } else if *self == Self::Idle {
            *self = Self::Tracking;
        }
        false
    }
}

impl Chord {
    pub(super) fn blocks_system_menu(self) -> bool {
        self.key.vk == 0 && matches!(self.key.modifiers, MOD_WIN | MOD_ALT)
    }

    pub(super) fn new(key: Hotkey) -> Self {
        Self {
            key,
            phase: Phase::Idle,
            foreign_held: KeySet::default(),
        }
    }

    pub(super) fn observe(
        &mut self,
        vk: u32,
        down: bool,
        mut is_down: impl FnMut(u32) -> bool,
        available: impl FnOnce() -> bool,
    ) -> bool {
        let key = self.key;
        let event_modifier = modifier(vk);
        let assigned = vk != 0 && (key.vk == vk || key.extra_keys.contains(vk));
        if event_modifier == 0 && !assigned {
            // Store only which foreign keys are currently down, including
            // ones pressed before the start of the shortcut. Key repeat does
            // not add another entry, and keyup immediately erases the bit.
            if ordinary(vk) {
                if down {
                    self.foreign_held.insert(vk);
                } else {
                    self.foreign_held.remove(vk);
                }
            }
            if matches!(self.phase, Phase::Tracking | Phase::Complete) {
                self.phase = Phase::Interrupted;
            }
            return false;
        }

        // Reconcile only possible held keys, and only on shortcut events.
        // This recovers if another hook consumed a foreign keyup. The event
        // being processed is a shortcut key, so GetAsyncKeyState for the
        // foreign keys already describes their post-event state.
        for foreign in self.foreign_held.iter() {
            if !is_down(foreign) {
                self.foreign_held.remove(foreign);
            }
        }

        let mut mods_before = 0;
        let mut mods_after = 0;
        for (left, right, bit) in [
            (VK_LMENU, VK_RMENU, MOD_ALT),
            (VK_LCONTROL, VK_RCONTROL, MOD_CONTROL),
            (VK_LSHIFT, VK_RSHIFT, MOD_SHIFT),
            (VK_LWIN, VK_RWIN, MOD_WIN),
        ] {
            for physical in [u32::from(left), u32::from(right)] {
                let was_down = is_down(physical);
                if was_down {
                    mods_before |= bit;
                }
                if if vk == physical { down } else { was_down } {
                    mods_after |= bit;
                }
            }
            // Low-level hooks normally supply side-specific modifiers.
            if event_modifier == bit && vk != u32::from(left) && vk != u32::from(right) {
                if down {
                    mods_after |= bit;
                } else {
                    mods_before |= bit;
                    mods_after &= !bit;
                }
            }
        }

        let mut held_before = false;
        let mut held_after = false;
        let mut all_held = true;
        for assigned in key.keys() {
            let was_down = is_down(assigned);
            let now_down = if assigned == vk { down } else { was_down };
            held_before |= was_down;
            held_after |= now_down;
            all_held &= now_down;
        }
        let clear_before = !held_before && mods_before == 0;
        let clear_after = !held_after && mods_after == 0;
        if clear_before && clear_after {
            self.phase = Phase::Idle;
            return false;
        }

        let interrupted = !self.foreign_held.is_empty()
            || (mods_after | mods_before) & !key.modifiers != 0
            || !available();
        self.phase.advance(
            clear_before,
            clear_after,
            all_held && mods_after == key.modifiers,
            interrupted,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: u32 = VK_LCONTROL as u32;
    const RCTRL: u32 = VK_RCONTROL as u32;
    const SHIFT: u32 = VK_LSHIFT as u32;
    const A: u32 = b'A' as u32;
    const B: u32 = b'B' as u32;
    const F23: u32 = VK_F23 as u32;
    const F24: u32 = VK_F24 as u32;

    struct Keyboard {
        chord: Chord,
        held: KeySet,
        available: bool,
    }

    impl Keyboard {
        fn new(key: Hotkey) -> Self {
            Self {
                chord: Chord::new(key),
                held: KeySet::default(),
                available: true,
            }
        }

        fn event(&mut self, vk: u32, down: bool) -> bool {
            let result =
                self.chord
                    .observe(vk, down, |key| self.held.contains(key), || self.available);
            if down {
                self.held.insert(vk);
            } else {
                self.held.remove(vk);
            }
            result
        }
    }

    #[test]
    fn modifier_only_waits_for_both_sides_and_cancels_on_regular_input() {
        let mut keyboard = Keyboard::new(Hotkey::new(MOD_CONTROL, 0));
        assert!(!keyboard.event(CTRL, true));
        assert!(!keyboard.event(RCTRL, true));
        assert!(!keyboard.event(CTRL, false));
        assert!(keyboard.event(RCTRL, false));
        assert!(!keyboard.event(CTRL, true));
        assert!(!keyboard.event(A, true));
        assert!(!keyboard.event(A, false));
        assert!(!keyboard.event(CTRL, false));
    }

    #[test]
    fn modifier_only_rejects_other_modifiers_and_input_started_in_rustart() {
        let mut keyboard = Keyboard::new(Hotkey::new(MOD_CONTROL, 0));
        assert!(!keyboard.event(SHIFT, true));
        assert!(!keyboard.event(CTRL, true));
        assert!(!keyboard.event(SHIFT, false));
        assert!(!keyboard.event(CTRL, false));
        keyboard.available = false;
        assert!(!keyboard.event(CTRL, true));
        keyboard.available = true;
        assert!(!keyboard.event(CTRL, false));
        assert!(!keyboard.event(CTRL, true));
        assert!(keyboard.event(CTRL, false));
    }

    #[test]
    fn two_key_assignment_requires_overlap_and_fires_once_after_release() {
        let mut key = Hotkey::new(0, F23);
        key.extra_keys.insert(F24);
        let mut keyboard = Keyboard::new(key);
        assert!(!keyboard.event(F23, true));
        assert!(!keyboard.event(F23, false));
        assert!(!keyboard.event(F24, true));
        assert!(!keyboard.event(F24, false));
        assert!(!keyboard.event(F24, true));
        assert!(!keyboard.event(F23, true));
        assert!(!keyboard.event(F23, true));
        assert!(!keyboard.event(F23, false));
        assert!(keyboard.event(F24, false));
        assert!(!keyboard.event(F24, false));
    }

    #[test]
    fn a_foreign_press_and_its_repeats_block_modifier_only_until_released() {
        let mut keyboard = Keyboard::new(Hotkey::new(MOD_CONTROL, 0));
        assert!(!keyboard.event(B, true));
        assert!(!keyboard.event(B, true));
        assert!(!keyboard.event(CTRL, true));
        assert!(!keyboard.event(CTRL, false));
        assert!(!keyboard.event(B, false));
        assert!(!keyboard.event(CTRL, true));
        assert!(keyboard.event(CTRL, false));
    }

    #[test]
    fn a_foreign_key_held_before_a_two_key_chord_interrupts_that_attempt() {
        let mut key = Hotkey::new(0, F23);
        key.extra_keys.insert(F24);
        let mut keyboard = Keyboard::new(key);
        assert!(!keyboard.event(A, true));
        assert!(!keyboard.event(F23, true));
        assert!(!keyboard.event(F24, true));
        // Dropping the unrelated key before both shortcut keys are released
        // must not retroactively turn this into a clean chord.
        assert!(!keyboard.event(A, false));
        assert!(!keyboard.event(F23, false));
        assert!(!keyboard.event(F24, false));
        assert!(!keyboard.event(F23, true));
        assert!(!keyboard.event(F24, true));
        assert!(!keyboard.event(F23, false));
        assert!(keyboard.event(F24, false));
    }

    #[test]
    fn asynchronous_state_recovers_a_missing_foreign_keyup() {
        let mut keyboard = Keyboard::new(Hotkey::new(MOD_CONTROL, 0));
        assert!(!keyboard.event(B, true));
        assert!(!keyboard.event(CTRL, true));
        assert!(keyboard.held.remove(B)); // The hook never receives this keyup.
        assert!(!keyboard.event(CTRL, false));
        assert!(keyboard.chord.foreign_held.is_empty());
        assert!(!keyboard.event(CTRL, true));
        assert!(keyboard.event(CTRL, false));
    }
}
