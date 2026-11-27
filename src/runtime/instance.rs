use super::{OWNER_CLASS, PING, QUIT, SHOW};
use crate::i18n::{Msg, render, tr};
use std::{
    ffi::OsString,
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr::null,
    thread,
    time::{Duration, Instant},
};

use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HWND,
            WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        System::Threading::{
            CREATE_NO_WINDOW, CreateMutexExW, CreateProcessW, GetExitCodeProcess, OpenMutexW,
            OpenProcess, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, STARTUPINFOW,
            SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
        },
        UI::{
            Shell::{
                IsUserAnAdmin, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
                SHELLEXECUTEINFOW, ShellExecuteExW,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

pub(super) const READY_TITLE: PCWSTR = w!("Rustart.Ready.1");
const ELECTION: PCWSTR = w!(r"Local\CKATEPTb.Rustart.Owner.v1");
const START_TIMEOUT: Duration = Duration::from_secs(8);
const EXIT_TIMEOUT: Duration = Duration::from_secs(4);
const ELEVATED_HELPER_TIMEOUT_MS: u32 = 16_000;
const POLL: Duration = Duration::from_millis(25);
fn with_error(message: Msg, error: impl std::fmt::Display) -> String {
    render(message, &[("error", &error.to_string())])
}

pub(super) fn elect() -> Result<Option<OwnedHandle>, String> {
    // Keeping the only handle open publishes the election until teardown.
    let raw = unsafe { CreateMutexExW(null(), ELECTION, 0, SYNCHRONIZATION_SYNCHRONIZE) };
    let status = unsafe { GetLastError() };
    if raw.is_null() {
        return if status == ERROR_ACCESS_DENIED {
            Ok(None)
        } else {
            Err(with_error(
                Msg::InstanceElectFailed,
                io::Error::from_raw_os_error(status as i32),
            ))
        };
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    Ok((status != ERROR_ALREADY_EXISTS).then_some(handle))
}

fn election_exists() -> Result<bool, String> {
    let raw = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, ELECTION) };
    if raw.is_null() {
        return match unsafe { GetLastError() } {
            ERROR_FILE_NOT_FOUND => Ok(false),
            ERROR_ACCESS_DENIED => Ok(true),
            status => Err(with_error(
                Msg::InstanceCheckLockFailed,
                io::Error::from_raw_os_error(status as i32),
            )),
        };
    }
    drop(unsafe { OwnedHandle::from_raw_handle(raw) });
    Ok(true)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Resident {
    hwnd: HWND,
    pid: u32,
}

impl Resident {
    fn find(title: PCWSTR) -> Option<Self> {
        let hwnd = unsafe { FindWindowW(OWNER_CLASS, title) };
        let mut pid = 0;
        if !hwnd.is_null() {
            unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        }
        (pid != 0).then_some(Self { hwnd, pid })
    }

    fn ready() -> Option<Self> {
        Self::find(READY_TITLE)
    }

    fn send(self, action: u32) -> Option<usize> {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(self.hwnd, &mut pid) };
        if pid != self.pid {
            return None;
        }
        let mut response = 0;
        if action == SHOW {
            unsafe { AllowSetForegroundWindow(self.pid) };
        }
        let delivered = unsafe {
            SendMessageTimeoutW(
                self.hwnd,
                action,
                0,
                0,
                SMTO_ABORTIFHUNG | SMTO_BLOCK | SMTO_ERRORONEXIT,
                3000,
                &mut response,
            )
        };
        (delivered != 0).then_some(response)
    }

    fn ping(self) -> Result<(), String> {
        match self.send(PING) {
            Some(1 | 2) => Ok(()),
            _ => Err(tr(Msg::InstanceNoResponse).into()),
        }
    }

    fn quit(self) -> Result<(), String> {
        // Open the process before asking it to quit, since the window can
        // disappear while the resident still holds the election mutex.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, self.pid) };
        let process = (!raw.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(raw) });
        let acknowledged = self.send(QUIT) == Some(1);
        if !acknowledged && Self::ready() == Some(self) {
            return Err(tr(Msg::InstanceNoResponse).into());
        }
        if let Some(process) = process {
            let millis = EXIT_TIMEOUT.as_millis() as u32;
            if unsafe { WaitForSingleObject(process.as_raw_handle(), millis) } == WAIT_OBJECT_0 {
                return Ok(());
            }
            return Err(tr(Msg::InstanceStopping).into());
        }
        // A standard client may be denied a handle to the elevated resident.
        let deadline = Instant::now() + EXIT_TIMEOUT;
        while Self::find(null()).is_some_and(|candidate| candidate.pid == self.pid) {
            if Instant::now() >= deadline {
                return Err(tr(Msg::InstanceStopping).into());
            }
            thread::sleep(POLL);
        }
        Ok(())
    }
}

fn resident_in_flight() -> Result<bool, String> {
    if Resident::find(null()).is_some() {
        return Ok(true);
    }
    election_exists()
}

fn launch() -> Result<OwnedHandle, String> {
    let exe: Vec<_> = executable()?.as_os_str().encode_wide().chain([0]).collect();
    let mut args: Vec<_> = "rustart.exe --resident".encode_utf16().chain([0]).collect();
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // A long-lived resident must never hold the client's redirected pipes open.
    if unsafe {
        CreateProcessW(
            exe.as_ptr(),
            args.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NO_WINDOW,
            null(),
            null(),
            &startup,
            &mut process,
        )
    } == 0
    {
        return Err(with_error(
            Msg::InstanceStartFailed,
            io::Error::last_os_error(),
        ));
    }
    drop(unsafe { OwnedHandle::from_raw_handle(process.hThread) });
    Ok(unsafe { OwnedHandle::from_raw_handle(process.hProcess) })
}

fn child_exit(process: &OwnedHandle) -> io::Result<Option<u32>> {
    match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
        WAIT_TIMEOUT => Ok(None),
        WAIT_OBJECT_0 => {
            let mut code = 0;
            if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(code))
        }
        _ => Err(io::Error::last_os_error()),
    }
}

fn executable() -> Result<std::path::PathBuf, String> {
    std::env::current_exe().map_err(|error| with_error(Msg::InstancePathFailed, error))
}
fn timed_out(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err(tr(Msg::InstanceStartTimeout).into())
    } else {
        Ok(())
    }
}

pub(crate) fn status() -> Result<Option<u32>, String> {
    Ok(await_resident()?.map(|resident| resident.pid))
}

pub(crate) struct InstallationOwner {
    resident: Resident,
    pub(crate) image: Option<PathBuf>,
}

impl InstallationOwner {
    pub(crate) fn quit(self) -> Result<bool, String> {
        if Resident::ready() != Some(self.resident) {
            return Ok(false);
        }
        self.resident.quit()?;
        Ok(true)
    }
}

pub(crate) fn installation_owner() -> Result<Option<InstallationOwner>, String> {
    Ok(await_resident()?.map(|resident| {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, resident.pid) };
        let image = if process.is_null() {
            None
        } else {
            let process = unsafe { OwnedHandle::from_raw_handle(process) };
            let mut path = vec![0u16; 32768];
            let mut length = path.len() as u32;
            let found = unsafe {
                QueryFullProcessImageNameW(
                    process.as_raw_handle(),
                    0,
                    path.as_mut_ptr(),
                    &mut length,
                )
            };
            (found != 0).then(|| PathBuf::from(OsString::from_wide(&path[..length as usize])))
        };
        InstallationOwner { resident, image }
    }))
}

fn await_resident() -> Result<Option<Resident>, String> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(resident) = Resident::ready() {
            match resident.ping() {
                Ok(()) => return Ok(Some(resident)),
                Err(error) if Resident::ready() == Some(resident) => return Err(error),
                Err(_) => {}
            }
        }
        if !resident_in_flight()? {
            return Ok(None);
        }
        timed_out(deadline)?;
        thread::sleep(POLL);
    }
}

pub(crate) fn client(action: u32, start: bool) -> Result<bool, String> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut child = None;
    loop {
        if let Some(resident) = Resident::ready() {
            if action == QUIT {
                return resident.quit().map(|()| true);
            }
            if action == PING {
                if resident.ping().is_ok() {
                    return Ok(true);
                }
            } else {
                match resident.send(action) {
                    Some(1) => return Ok(true),
                    // The resident brought its active settings window forward.
                    Some(3) if action == SHOW => return Ok(true),
                    Some(4) if action == SHOW => {
                        return Err(tr(Msg::InstanceFocusFailed).into());
                    }
                    _ => {}
                }
            }
            if Resident::ready() == Some(resident) {
                return Err(tr(Msg::InstanceNoResponse).into());
            }
        }
        if let Some(process) = &child
            && let Some(code) = child_exit(process)
                .map_err(|error| with_error(Msg::InstanceCheckStartFailed, error))?
        {
            child = None;
            if code != 0 {
                return Err(with_error(Msg::InstanceStartFailed, code));
            }
        }

        timed_out(deadline)?;
        if child.is_none() && !resident_in_flight()? {
            if !start {
                return Err(tr(Msg::InstanceNotRunning).into());
            }
            child = Some(launch()?);
        }
        thread::sleep(POLL);
    }
}

pub(crate) fn elevated_startup() -> Result<bool, String> {
    // The first instance owns this session until the user quits it. A later
    // startup command only signals the resident, regardless of its privileges.
    if await_resident()?.is_some() {
        return Ok(true);
    }
    if unsafe { IsUserAnAdmin() } != 0 {
        return elevated_background();
    }

    let path = executable()?;
    let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut launch = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: path.as_ptr(),
        lpParameters: w!("--elevated-background"),
        nShow: SW_HIDE,
        ..Default::default()
    };
    if unsafe { ShellExecuteExW(&mut launch) } == 0 {
        return Err(with_error(
            Msg::InstanceElevatedStartFailedDetail,
            io::Error::last_os_error(),
        ));
    }

    if !launch.hProcess.is_null() {
        let process = unsafe { OwnedHandle::from_raw_handle(launch.hProcess) };
        let mut status = 1;
        let complete =
            unsafe { WaitForSingleObject(process.as_raw_handle(), ELEVATED_HELPER_TIMEOUT_MS) }
                == WAIT_OBJECT_0;
        if !complete
            || unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut status) } == 0
            || status != 0
        {
            return Err(tr(Msg::InstanceElevatedStartFailed).into());
        }
        return Ok(true);
    }

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if Resident::ready().is_some_and(|resident| resident.ping().is_ok()) {
            return Ok(true);
        }
        timed_out(deadline)?;
        thread::sleep(POLL);
    }
}

pub(crate) fn elevated_background() -> Result<bool, String> {
    if unsafe { IsUserAnAdmin() } == 0 {
        return Err(tr(Msg::InstanceElevationRequired).into());
    }
    client(PING, true)
}
