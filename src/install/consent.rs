use std::io;
use std::os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use std::ptr::{null, null_mut};

use windows_sys::{
    Win32::{
        Foundation::{ERROR_CANCELLED, WAIT_OBJECT_0},
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::{
            Com::{
                COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
            },
            Threading::{
                GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken,
                WaitForSingleObject,
            },
        },
        UI::{
            Shell::{
                SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
                ShellExecuteExW,
            },
            WindowsAndMessaging::SW_HIDE,
        },
    },
    core::w,
};

pub(crate) fn elevated() -> io::Result<bool> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut info = TOKEN_ELEVATION::default();
    let mut bytes = 0;
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            (&raw mut info).cast(),
            size_of_val(&info) as u32,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(info.TokenIsElevated != 0)
}

pub(crate) fn request(parameters: &str) -> io::Result<Option<u32>> {
    let image = std::env::current_exe()?;
    let path: Vec<_> = image.as_os_str().encode_wide().chain([0]).collect();
    let parameters: Vec<_> = parameters.encode_utf16().chain([0]).collect();
    let apartment = unsafe {
        CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    if apartment < 0 {
        return Err(io::Error::other(format!(
            "Could not initialize the installer: {apartment:#x}"
        )));
    }
    let mut launch = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("runas"),
        lpFile: path.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_HIDE,
        ..Default::default()
    };
    let error = (unsafe { ShellExecuteExW(&mut launch) } == 0).then(io::Error::last_os_error);
    unsafe { CoUninitialize() };
    if let Some(error) = error {
        return if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    if launch.hProcess.is_null() {
        return Err(io::Error::other(
            "Could not obtain the Rustart installer process.",
        ));
    }
    let child = unsafe { OwnedHandle::from_raw_handle(launch.hProcess) };
    let mut status = 0;
    if unsafe { WaitForSingleObject(child.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0
        || unsafe { GetExitCodeProcess(child.as_raw_handle(), &mut status) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(status))
}
