//! Console ownership and download origin shared by the package and its payload.
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType};
use windows_sys::Win32::System::Console::{
    FreeConsole, GetConsoleProcessList, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

pub(crate) const PACKAGE_ORIGIN: &str = "RUSTART_PACKAGE_ORIGIN";

pub(crate) fn configure_graphical_child(command: &mut Command) {
    // Detaching from a private console can leave stale standard handles.
    // Give the child valid handles without allocating a console window.
    command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
}

pub(crate) fn launched_from_terminal() -> bool {
    let mut pid = 0;
    let attached = unsafe { GetConsoleProcessList(&mut pid, 1) };
    let redirected = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .iter()
        .any(|&stream| {
            matches!(
                unsafe { GetFileType(GetStdHandle(stream)) },
                FILE_TYPE_DISK | FILE_TYPE_PIPE
            )
        });
    // Older Windows versions can allocate a private console for Explorer.
    if attached == 1 && !redirected {
        unsafe { FreeConsole() };
    }
    attached > 1 || redirected
}
