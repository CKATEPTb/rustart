use std::{
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr::null_mut,
};

use windows_sys::{
    Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY,
            RRF_SUBKEY_WOW6464KEY, RegGetValueW,
        },
    },
    core::{PCWSTR, w},
};

fn registry_path(hive: HKEY, key: PCWSTR, field: PCWSTR, view: u32) -> Option<PathBuf> {
    let mut value = vec![0u16; 32_768];
    let mut bytes = (value.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            hive,
            key,
            field,
            RRF_RT_REG_SZ | view,
            null_mut(),
            value.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let value = value.get(..bytes as usize / 2)?.strip_suffix(&[0])?;
    if value.contains(&0) {
        return None;
    }
    let path = PathBuf::from(OsString::from_wide(value));
    path.is_absolute().then_some(path)
}

fn is_client(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("steam.exe"))
        && path.is_file()
}

pub(in crate::catalog::native) fn executable() -> Option<PathBuf> {
    let valve = w!("Software\\Valve\\Steam");
    registry_path(HKEY_CURRENT_USER, valve, w!("SteamExe"), 0)
        .filter(|path| is_client(path))
        .or_else(|| {
            registry_path(HKEY_CURRENT_USER, valve, w!("SteamPath"), 0)
                .map(|path| path.join("steam.exe"))
                .filter(|path| is_client(path))
        })
        .or_else(|| {
            [RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY]
                .into_iter()
                .filter_map(|view| {
                    registry_path(HKEY_LOCAL_MACHINE, valve, w!("InstallPath"), view)
                })
                .map(|path| path.join("steam.exe"))
                .find(|path| is_client(path))
        })
        .or_else(|| {
            ["ProgramFiles(x86)", "ProgramFiles"]
                .into_iter()
                .filter_map(std::env::var_os)
                .map(|root| PathBuf::from(root).join(r"Steam\steam.exe"))
                .find(|path| path.is_absolute() && is_client(path))
        })
}

pub(crate) fn steam_icon(app_id: u32) -> Option<PathBuf> {
    let key: Vec<_> =
        format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Steam App {app_id}")
            .encode_utf16()
            .chain([0])
            .collect();
    [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY]
        .into_iter()
        .filter_map(|view| registry_path(HKEY_LOCAL_MACHINE, key.as_ptr(), w!("DisplayIcon"), view))
        .find(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ico"))
                && path.is_file()
        })
        .or_else(executable)
}
