use std::{ffi::OsStr, io, mem::ManuallyDrop, os::windows::ffi::OsStrExt, path::Path};

use windows::{
    Win32::{
        System::{
            Com::{
                CLSCTX_ALL, CoAllowSetForegroundWindow, CoCreateInstance, IDispatch,
                IServiceProvider,
            },
            Variant::{
                VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BSTR, VT_I4, VariantClear,
            },
        },
        UI::Shell::{
            CSIDL_DESKTOP, IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows,
            SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
        },
    },
    core::{BSTR, Error, Interface},
};
use windows_sys::Win32::{
    Foundation::{E_INVALIDARG, HWND},
    UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId, SW_HIDE, SW_SHOWNORMAL},
};

use super::privilege::{self, Context, Process, unverified};

pub(super) struct StandardDesktop {
    dispatch: IShellDispatch2,
    executable: BSTR,
    pub(super) context: Context,
}

impl StandardDesktop {
    pub(super) fn find(session: u32) -> io::Result<Self> {
        let shell = unsafe { GetShellWindow() };
        let pid = window_pid(shell)?;
        let process = Process::open(pid)?;
        let executable = privilege::explorer_image()?;
        if !privilege::paths_equal(&process.image()?, &executable) {
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
        let executable = BSTR::from_wide(&executable.as_os_str().encode_wide().collect::<Vec<_>>());
        Ok(Self {
            dispatch,
            executable,
            context,
        })
    }

    pub(super) fn start_worker(
        &self,
        executable: &Path,
        channel: &str,
    ) -> windows::core::Result<()> {
        let path: Vec<_> = executable.as_os_str().encode_wide().collect();
        self.shell_execute(
            &BSTR::from_wide(&path),
            BSTR::from(format!("--launch-normal-worker {channel}")),
            SW_HIDE,
        )
    }

    fn shell_execute(&self, file: &BSTR, args: BSTR, show: i32) -> windows::core::Result<()> {
        let mut args = string_variant(args);
        let empty = VARIANT::default();
        unsafe {
            let result =
                self.dispatch
                    .ShellExecute(file, &args, &empty, &empty, &integer_variant(show));
            VariantClear(&mut args)?;
            result
        }
    }

    pub(super) fn explore(&self, folder: &Path) -> windows::core::Result<()> {
        self.open_explorer(folder.as_os_str(), false)
    }

    pub(super) fn select(&self, item: &OsStr) -> windows::core::Result<()> {
        self.open_explorer(item, true)
    }

    fn open_explorer(&self, target: &OsStr, select: bool) -> windows::core::Result<()> {
        let args = explorer_arguments(target, select)?;
        // The medium-integrity helper starts with SW_HIDE. Give the folder
        // window its own show state and let Explorer activate it.
        unsafe {
            let _ = CoAllowSetForegroundWindow(&self.dispatch, None);
        }
        self.shell_execute(&self.executable, args, SW_SHOWNORMAL)
    }
}

fn window_pid(hwnd: HWND) -> io::Result<u32> {
    let mut pid = 0;
    if hwnd.is_null() || unsafe { GetWindowThreadProcessId(hwnd, &mut pid) } == 0 || pid == 0 {
        return Err(unverified("could not identify the desktop shell window"));
    }
    Ok(pid)
}

fn explorer_arguments(target: &OsStr, select: bool) -> windows::core::Result<BSTR> {
    let target: Vec<_> = target.encode_wide().collect();
    let quote = u16::from(b'"');
    let backslash = u16::from(b'\\');
    if target.is_empty() || target.iter().any(|&unit| unit == 0 || unit == quote) {
        return Err(Error::from_hresult(windows::core::HRESULT(E_INVALIDARG)));
    }
    let mut args: Vec<_> = if select {
        "/select,".encode_utf16().collect()
    } else {
        Vec::new()
    };
    args.push(quote);
    args.extend_from_slice(&target);
    // Inside a quoted argument, trailing backslashes must be doubled so the
    // closing quote is preserved (for example when opening a drive root).
    args.extend(target.iter().rev().take_while(|&&unit| unit == backslash));
    args.push(quote);
    Ok(BSTR::from_wide(&args))
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
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};

    #[test]
    fn explorer_arguments_keep_names_in_a_single_quoted_field() {
        assert_eq!(
            explorer_arguments(OsStr::new(r"C:\Program Files\Пример, App.exe"), true)
                .unwrap()
                .to_string(),
            r#"/select,"C:\Program Files\Пример, App.exe""#
        );
        assert_eq!(
            explorer_arguments(OsStr::new(r"C:\"), false)
                .unwrap()
                .to_string(),
            r#""C:\\""#
        );
        for invalid in ["", "a\0b", "a\"b"] {
            assert!(explorer_arguments(OsStr::new(invalid), true).is_err());
        }

        let mut raw: Vec<_> = r"C:\Long\App".encode_utf16().collect();
        raw.push(0xd800);
        let args = explorer_arguments(&OsString::from_wide(&raw), true).unwrap();
        assert_eq!(&args[9..args.len() - 1], raw);
    }

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
