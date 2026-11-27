use std::{io, mem::ManuallyDrop, os::windows::ffi::OsStrExt, path::Path};

use windows::{
    Win32::{
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance, IDispatch, IServiceProvider},
            Variant::{
                VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BSTR, VT_I4, VariantClear,
            },
        },
        UI::Shell::{
            CSIDL_DESKTOP, IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows,
            SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
        },
    },
    core::{BSTR, Interface},
};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId, SW_HIDE},
};

use super::privilege::{self, Context, Process, unverified};

pub(super) struct StandardDesktop {
    dispatch: IShellDispatch2,
    pub(super) context: Context,
}

impl StandardDesktop {
    pub(super) fn find(session: u32) -> io::Result<Self> {
        let shell = unsafe { GetShellWindow() };
        let pid = window_pid(shell)?;
        let process = Process::open(pid)?;
        if !privilege::paths_equal(&process.image()?, &privilege::explorer_image()?) {
            return Err(unverified("the desktop shell is not Windows Explorer"));
        }
        let context = process.context()?;
        if context.session != session || !context.standard() {
            return Err(unverified(
                "Windows Explorer does not have a standard token for this session",
            ));
        }
        process.verify_interactive(&context)?;
        let (browser_window, dispatch) =
            desktop_dispatch().map_err(|error| io::Error::other(error.to_string()))?;
        if unsafe { GetShellWindow() } != shell || window_pid(browser_window)? != pid {
            return Err(unverified("the desktop shell changed during verification"));
        }
        Ok(Self { dispatch, context })
    }

    pub(super) fn start_worker(
        &self,
        executable: &Path,
        channel: &str,
    ) -> windows::core::Result<()> {
        let path: Vec<_> = executable.as_os_str().encode_wide().collect();
        let mut args = string_variant(BSTR::from(format!("--launch-normal-worker {channel}")));
        let empty = VARIANT::default();
        unsafe {
            let result = self.dispatch.ShellExecute(
                &BSTR::from_wide(&path),
                &args,
                &empty,
                &empty,
                &integer_variant(SW_HIDE),
            );
            VariantClear(&mut args)?;
            result
        }
    }
}

fn window_pid(hwnd: HWND) -> io::Result<u32> {
    let mut pid = 0;
    if hwnd.is_null() || unsafe { GetWindowThreadProcessId(hwnd, &mut pid) } == 0 || pid == 0 {
        return Err(unverified("could not identify the desktop shell window"));
    }
    Ok(pid)
}

pub(super) fn explore(desktop: &StandardDesktop, folder: &Path) -> windows::core::Result<()> {
    let path: Vec<_> = folder.as_os_str().encode_wide().collect();
    let mut directory = string_variant(BSTR::from_wide(&path));
    unsafe {
        let result = desktop.dispatch.Explore(&directory);
        VariantClear(&mut directory)?;
        result
    }
}

fn string_variant(string: BSTR) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BSTR,
                Anonymous: VARIANT_0_0_0 {
                    bstrVal: ManuallyDrop::new(string),
                },
                ..Default::default()
            }),
        },
    }
}

fn desktop_dispatch() -> windows::core::Result<(HWND, IShellDispatch2)> {
    // Get the Shell.Application object from the desktop view running in Explorer.
    // Creating Shell.Application directly could execute in the elevated caller.
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
        let mut hwnd = 0;
        let desktop = windows.FindWindowSW(
            &integer_variant(CSIDL_DESKTOP as i32),
            &VARIANT::default(),
            SWC_DESKTOP,
            &mut hwnd,
            SWFO_NEEDDISPATCH,
        )?;
        let service: IServiceProvider = desktop.cast()?;
        let browser: IShellBrowser = service.QueryService(&SID_STopLevelBrowser)?;
        let window = browser.GetWindow()?.0;
        let view = browser.QueryActiveShellView()?;
        let automation: IDispatch = view.GetItemObject(SVGIO_BACKGROUND)?;
        let folder: IShellFolderViewDual = automation.cast()?;
        Ok((window, folder.Application()?.cast()?))
    }
}

fn integer_variant(value: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                Anonymous: VARIANT_0_0_0 { lVal: value },
                ..Default::default()
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an interactive desktop with an ordinary Explorer shell"]
    fn verifies_the_interactive_explorer_without_opening_anything() {
        let _apartment = super::super::Apartment::enter().unwrap();
        let current = privilege::current().unwrap();
        let desktop = StandardDesktop::find(current.session).unwrap();
        assert!(desktop.context.standard());
        eprintln!(
            "Verified medium Explorer for session {}",
            desktop.context.session
        );
    }
}
