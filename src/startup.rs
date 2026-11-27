mod consent;
mod scheduler;

use scheduler::State;

use crate::i18n::{Msg, render, tr};
use std::{
    io,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::null_mut,
};

use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
            ERROR_UNSUPPORTED_TYPE,
        },
        Globalization::{CSTR_EQUAL, CompareStringOrdinal},
        System::Registry::*,
    },
    core::{PCWSTR, w},
};

const RUN: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const APPROVAL: PCWSTR =
    w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run");
const VALUE: PCWSTR = w!("Rustart");
const COMMAND_LIMIT: usize = 260;
pub(crate) const USER_LOGON: &str = "--user-logon";
pub(crate) const ADMIN_LOGON: &str = "--admin-logon";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Standard,
    Administrator,
    DisabledByWindows,
}

fn missing(status: u32) -> bool {
    matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
}

fn failure(error: impl std::fmt::Display) -> String {
    render(Msg::StartupFailed, &[("error", &error.to_string())])
}

fn registry_error(status: u32) -> String {
    failure(io::Error::from_raw_os_error(status as i32))
}

struct Registration {
    run: PCWSTR,
    approval: PCWSTR,
    exe: PathBuf,
}

impl Registration {
    fn current() -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        Ok(Self {
            run: RUN,
            approval: APPROVAL,
            exe,
        })
    }

    fn read_command(&self) -> Result<Option<Vec<u16>>, String> {
        let mut raw = [0u16; COMMAND_LIMIT + 1];
        let mut bytes = size_of_val(&raw) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                self.run,
                VALUE,
                RRF_RT_REG_SZ | RRF_NOEXPAND,
                null_mut(),
                raw.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if missing(status) {
            return Ok(None);
        }
        if matches!(status, ERROR_MORE_DATA | ERROR_UNSUPPORTED_TYPE) {
            return Err(tr(Msg::StartupOccupied).into());
        }
        if status != ERROR_SUCCESS {
            return Err(registry_error(status));
        }
        Ok(Some(raw[..bytes as usize / 2].to_vec()))
    }

    fn registered(&self, actual: Option<&[u16]>) -> Result<bool, String> {
        let Some(actual) = actual else {
            return Ok(false);
        };
        let expected = command(&self.exe);
        let split = expected.len() - USER_LOGON.len() - 2;
        if actual.get(split..) == Some(&expected[split..])
            && unsafe {
                CompareStringOrdinal(
                    expected.as_ptr(),
                    split as i32,
                    actual.as_ptr(),
                    split as i32,
                    1,
                ) == CSTR_EQUAL
            }
        {
            Ok(true)
        } else {
            Err(tr(Msg::StartupOccupied).into())
        }
    }

    fn windows_allows(&self) -> Result<bool, String> {
        // The Explorer marker is absent for apps which the user hasn't changed.
        // Like Rustile, leave the user's Windows setting in place when editing Run.
        let mut marker = [0u8; 12];
        let mut bytes = size_of_val(&marker) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                self.approval,
                VALUE,
                RRF_RT_REG_BINARY,
                null_mut(),
                marker.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if missing(status) {
            return Ok(true);
        }
        if status != ERROR_SUCCESS {
            return Err(registry_error(status));
        }
        Ok(bytes >= 4 && marker[0] & 1 == 0)
    }

    fn read(&self, task: State) -> Result<Mode, String> {
        if task == State::Active {
            return Ok(Mode::Administrator);
        }
        let actual = self.read_command()?;
        if self.registered(actual.as_deref())? {
            return if self.windows_allows()? {
                Ok(Mode::Standard)
            } else {
                Ok(Mode::DisabledByWindows)
            };
        }
        Ok(Mode::Off)
    }

    fn replace(&self, previous: Option<&[u16]>, next: Option<&[u16]>) -> Result<(), String> {
        if self.read_command()?.as_deref() != previous {
            return Err(tr(Msg::StartupOccupied).into());
        }
        if previous == next {
            return Ok(());
        }
        let status = if let Some(data) = next {
            if data.len() > COMMAND_LIMIT + 1 {
                return Err(tr(Msg::StartupPathLong).into());
            }
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    self.run,
                    VALUE,
                    REG_SZ,
                    data.as_ptr().cast(),
                    size_of_val(data) as u32,
                )
            }
        } else {
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, self.run, VALUE) }
        };
        if status == ERROR_SUCCESS || (next.is_none() && missing(status)) {
            Ok(())
        } else {
            Err(registry_error(status))
        }
    }

    fn write(
        &self,
        mode: Mode,
        state: impl Fn() -> Result<State, String>,
        configure: impl FnOnce(bool) -> Result<bool, String>,
    ) -> Result<(), String> {
        let task = state()?;
        let actual = self.read_command()?;
        let previous = actual.as_deref();
        let registered = self.registered(previous)?;
        let has_task = matches!(task, State::Active | State::Disabled);
        let normal = command(&self.exe);
        let normal = Some(normal.as_slice());
        match mode {
            Mode::Administrator => {
                if task == State::Active {
                    return Ok(());
                }
                if !registered {
                    return Err(tr(Msg::StartupEnableFirst).into());
                }
                if !self.windows_allows()? {
                    return Err(tr(Msg::StartupDisabledHelp).into());
                }
                if task == State::Unavailable {
                    return Err(tr(Msg::StartupTaskCreate).into());
                }
                // The guarded Run command yields to an active task even if
                // the process exits before it can remove the obsolete entry.
                self.replace(previous, normal)?;
                match configure(true) {
                    Ok(true) => {}
                    result => {
                        self.replace(normal, previous)?;
                        return result.map(|_| ());
                    }
                }
                match state()? {
                    State::Active => self.replace(normal, None)?,
                    State::Absent | State::Disabled => {
                        self.replace(normal, previous)?;
                        return Err(tr(Msg::StartupTaskCreate).into());
                    }
                    State::Unavailable => return Err(tr(Msg::StartupTaskCreate).into()),
                }
            }
            Mode::Standard => {
                let allowed = self.windows_allows()?;
                if !allowed && (has_task || registered) {
                    return Err(tr(Msg::StartupDisabledHelp).into());
                }
                self.replace(previous, normal)?;
                if !allowed {
                    return Err(tr(Msg::StartupDisabledHelp).into());
                }
                if has_task {
                    let result = configure(false);
                    match state() {
                        Ok(State::Absent) => {}
                        state => {
                            self.replace(normal, previous)?;
                            if result? {
                                state?;
                                return Err(tr(Msg::StartupTaskRemove).into());
                            }
                        }
                    }
                }
            }
            Mode::Off => {
                if has_task {
                    if !configure(false)? {
                        return Ok(());
                    }
                    if state()? != State::Absent {
                        return Err(tr(Msg::StartupTaskRemove).into());
                    }
                }
                self.replace(previous, None)?;
            }
            Mode::DisabledByWindows => return Err(tr(Msg::StartupDisabledHelp).into()),
        }
        Ok(())
    }
}

fn command(exe: &Path) -> Vec<u16> {
    "\"".encode_utf16()
        .chain(exe.as_os_str().encode_wide())
        .chain("\"".encode_utf16())
        .chain([b' ' as u16])
        .chain(USER_LOGON.encode_utf16())
        .chain([0])
        .collect()
}

pub fn read() -> Result<Mode, String> {
    Registration::current()?.read(scheduler::state(&consent::sid()?)?)
}

pub fn write(mode: Mode) -> Result<(), String> {
    let sid = consent::sid()?;
    Registration::current()?.write(
        mode,
        || scheduler::state(&sid),
        |enable| consent::configure(enable, &sid),
    )
}

pub(crate) fn handle_helper(args: &[String]) -> Option<std::process::ExitCode> {
    consent::handle_helper(args)
}

pub(crate) fn run_at_logon() -> Result<bool, String> {
    let registration = Registration::current()?;
    let actual = registration.read_command()?;
    if !registration.registered(actual.as_deref())? || !registration.windows_allows()? {
        return Ok(false);
    }
    // Ordinary startup remains available if Task Scheduler cannot be reached.
    match scheduler::state(&consent::sid()?) {
        Ok(State::Active) => Ok(false),
        Ok(_) => Ok(true),
        Err(detail) => {
            eprintln!("{detail}");
            Ok(true)
        }
    }
}

pub(crate) fn scheduled_at_logon() -> Result<bool, String> {
    use crate::install::{consent, files};
    Ok(consent::elevated().map_err(failure)?
        && files::is_installed_image().map_err(failure)?
        && scheduler::state(&self::consent::sid()?)? == State::Active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestKey(Vec<u16>);

    impl TestKey {
        fn new(kind: &str) -> Self {
            let tick = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let name = format!(
                r"Software\Rustart.Test.{}.{tick}.{kind}",
                std::process::id()
            );
            Self(name.encode_utf16().chain([0]).collect())
        }

        fn as_ptr(&self) -> PCWSTR {
            self.0.as_ptr()
        }
    }

    impl Drop for TestKey {
        fn drop(&mut self) {
            unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, self.as_ptr()) };
        }
    }

    fn fixture(run: &TestKey, approval: &TestKey, exe: &str) -> Registration {
        Registration {
            run: run.as_ptr(),
            approval: approval.as_ptr(),
            exe: PathBuf::from(exe),
        }
    }

    fn block_in_windows(settings: &Registration) {
        let marker = [3_u8, 0, 0, 0];
        assert_eq!(
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    settings.approval,
                    VALUE,
                    REG_BINARY,
                    marker.as_ptr().cast(),
                    marker.len() as u32,
                )
            },
            ERROR_SUCCESS
        );
    }

    #[test]
    fn startup_registration_rejects_an_unexpected_argument() {
        let run = TestKey::new("UnexpectedRun");
        let approval = TestKey::new("UnexpectedApproval");
        let settings = fixture(
            &run,
            &approval,
            r"C:\Program Files\CKATEPTb\Rustart\rustart.exe",
        );
        let unexpected = format!("\"{}\" --admin-logon", settings.exe.display());
        let unexpected: Vec<_> = unexpected.encode_utf16().chain([0]).collect();
        settings.replace(None, Some(&unexpected)).unwrap();

        assert_eq!(
            settings.read(State::Absent).unwrap_err(),
            tr(Msg::StartupOccupied)
        );
        for mode in [Mode::Off, Mode::Standard, Mode::Administrator] {
            assert_eq!(
                settings
                    .write(mode, || Ok(State::Absent), |_| unreachable!())
                    .unwrap_err(),
                tr(Msg::StartupOccupied)
            );
        }
        assert_eq!(settings.read_command().unwrap(), Some(unexpected));
    }

    #[test]
    fn cancellation_and_scheduler_errors_preserve_previous_startup() {
        use std::cell::Cell;
        let run = TestKey::new("CancelRun");
        let approval = TestKey::new("CancelApproval");
        let settings = fixture(&run, &approval, r"C:\Тест Rustart\rustart.exe");
        let task = Cell::new(State::Absent);
        let state = || Ok(task.get());
        assert!(
            settings
                .write(Mode::Administrator, state, |_| unreachable!())
                .is_err()
        );
        assert_eq!(settings.read(State::Disabled).unwrap(), Mode::Off);
        let previous = command(Path::new(r"C:\ТЕСТ RUSTART\RUSTART.EXE"));
        settings.replace(None, Some(&previous)).unwrap();
        assert_eq!(settings.read(task.get()).unwrap(), Mode::Standard);
        assert_eq!(
            settings
                .write(
                    Mode::Administrator,
                    || Ok(State::Unavailable),
                    |_| unreachable!()
                )
                .unwrap_err(),
            tr(Msg::StartupTaskCreate)
        );

        settings
            .write(Mode::Administrator, state, |_| {
                assert_eq!(
                    settings.read_command().unwrap(),
                    Some(command(&settings.exe))
                );
                Ok(false)
            })
            .unwrap();
        assert_eq!(settings.read_command().unwrap(), Some(previous.clone()));
        assert!(
            settings
                .write(Mode::Administrator, state, |_| Ok(true))
                .is_err()
        );
        assert_eq!(settings.read_command().unwrap(), Some(previous));

        settings
            .write(Mode::Administrator, state, |enable| {
                assert!(enable);
                task.set(State::Active);
                Ok(true)
            })
            .unwrap();
        assert_eq!(settings.read(task.get()).unwrap(), Mode::Administrator);
        assert_eq!(settings.read_command().unwrap(), None);

        settings
            .write(Mode::Standard, state, |enable| {
                assert!(!enable);
                assert_eq!(
                    settings.read_command().unwrap(),
                    Some(command(&settings.exe))
                );
                Ok(false)
            })
            .unwrap();
        assert_eq!(settings.read_command().unwrap(), None);
        assert!(settings.write(Mode::Standard, state, |_| Ok(true)).is_err());
        assert_eq!(settings.read_command().unwrap(), None);
        assert_eq!(settings.read(task.get()).unwrap(), Mode::Administrator);
        settings.write(Mode::Off, state, |_| Ok(false)).unwrap();
        assert_eq!(settings.read(task.get()).unwrap(), Mode::Administrator);

        settings
            .write(Mode::Standard, state, |enable| {
                assert!(!enable);
                task.set(State::Absent);
                Ok(true)
            })
            .unwrap();
        assert_eq!(settings.read(task.get()).unwrap(), Mode::Standard);
        settings
            .write(Mode::Off, state, |_| unreachable!())
            .unwrap();
        assert_eq!(settings.read(task.get()).unwrap(), Mode::Off);
    }

    #[test]
    fn startup_approval_and_foreign_entries_are_preserved() {
        let run = TestKey::new("Run");
        let approval = TestKey::new("Approval");
        let settings = fixture(&run, &approval, r"C:\Тест Rustart\rustart.exe");
        let absent = || Ok(State::Absent);
        let unavailable = || Ok(State::Unavailable);
        assert_eq!(settings.read(State::Absent).unwrap(), Mode::Off);
        settings
            .write(Mode::Standard, unavailable, |_| unreachable!())
            .unwrap();
        assert_eq!(settings.read(State::Unavailable).unwrap(), Mode::Standard);
        block_in_windows(&settings);
        assert_eq!(
            settings.read(State::Absent).unwrap(),
            Mode::DisabledByWindows
        );
        assert!(
            settings
                .write(Mode::Administrator, absent, |_| unreachable!())
                .is_err()
        );
        settings
            .write(Mode::Off, absent, |_| unreachable!())
            .unwrap();
        assert_eq!(settings.read(State::Absent).unwrap(), Mode::Off);
        assert!(
            settings
                .write(Mode::Standard, absent, |_| unreachable!())
                .is_err()
        );
        assert_eq!(
            settings.read(State::Absent).unwrap(),
            Mode::DisabledByWindows
        );
        let previous = settings.read_command().unwrap();
        let other = command(Path::new(r"C:\Another Project\rustart.exe"));
        settings.replace(previous.as_deref(), Some(&other)).unwrap();
        assert_eq!(
            settings.read(State::Absent).unwrap_err(),
            tr(Msg::StartupOccupied)
        );
        assert_eq!(
            settings
                .write(Mode::Off, absent, |_| unreachable!())
                .unwrap_err(),
            tr(Msg::StartupOccupied)
        );
        assert_eq!(settings.read_command().unwrap(), Some(other));
    }
}
