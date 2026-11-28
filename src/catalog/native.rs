mod explorer;
mod internet_shortcut;
mod launch_channel;
mod packages;
mod privilege;
mod reveal;
mod shortcut;
mod standard_launch;
mod steam;
pub(crate) use privilege::current_user_sid;
pub use reveal::reveal;
pub use standard_launch::worker as standard_launch_worker;
pub(crate) use steam::{steam_icon, steam_library_folders};

use std::{
    collections::HashSet,
    ffi::OsString,
    fs, io,
    mem::size_of,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use windows::{
    Win32::{
        Foundation::RPC_E_CHANGED_MODE,
        System::Com::{
            CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
            CoAllowSetForegroundWindow, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
            CoUninitialize,
        },
        UI::Shell::{
            AO_NONE, ApplicationActivationManager, BHID_EnumItems, FOLDERID_AppsFolder,
            FOLDERID_CommonPrograms, FOLDERID_CommonStartup, FOLDERID_Programs, FOLDERID_Startup,
            IApplicationActivationManager, IEnumShellItems, IShellItem, KF_FLAG_DEFAULT,
            SHCreateItemFromParsingName, SHGetIDListFromObject, SHGetKnownFolderItem,
            SHGetKnownFolderPath, SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
        },
    },
    core::{GUID, PCWSTR, PWSTR, w},
};
use windows_sys::Win32::{
    Foundation::{ERROR_NO_ASSOCIATION, ERROR_NOT_SUPPORTED},
    Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
    UI::{
        Shell::{
            SEE_MASK_FLAG_NO_UI, SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
            ShellExecuteExW,
        },
        WindowsAndMessaging::SW_SHOWNORMAL,
    },
};

use super::{
    App, Source, describe_name_collisions,
    identity::{Identity, packaged_id},
};
use crate::i18n::{Msg, render, tr};

struct Apartment(bool);

impl Apartment {
    fn enter() -> windows::core::Result<Self> {
        let status =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if status == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        status.ok()?;
        Ok(Self(true))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

pub fn discover() -> Result<Vec<App>, String> {
    let _apartment = Apartment::enter()
        .map_err(|error| format!("Не удалось подключиться к оболочке Windows. {error}"))?;
    let mut apps = Vec::new();
    let mut errors = Vec::new();
    let mut seen_shortcuts = HashSet::new();
    let mut identities = shortcut::Index::new();
    for (folder, common) in [
        (&FOLDERID_Programs, false),
        (&FOLDERID_CommonPrograms, true),
    ] {
        if let Err(error) = desktop_apps(
            folder,
            common,
            &mut apps,
            &mut seen_shortcuts,
            &mut identities,
        ) {
            errors.push(error);
        }
    }
    if let Err(error) = packaged_apps(&mut apps, &mut identities) {
        errors.push(error.to_string());
    }
    steam::discover(&mut apps);
    if apps.is_empty() && !errors.is_empty() {
        return Err(format!(
            "Не удалось загрузить список приложений. {}",
            errors.join(" / ")
        ));
    }
    apps.sort_by(|left, right| left.search_name.cmp(&right.search_name));
    describe_name_collisions(&mut apps);
    Ok(apps)
}

fn desktop_apps(
    folder: &GUID,
    common: bool,
    apps: &mut Vec<App>,
    seen_shortcuts: &mut HashSet<PathBuf>,
    identities: &mut shortcut::Index,
) -> Result<(), String> {
    let root = unsafe { SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT, None) }
        .map_err(|error| error.to_string())?;
    let root = PathBuf::from(owned_string(root));
    let startup_folder = if common {
        &FOLDERID_CommonStartup
    } else {
        &FOLDERID_Startup
    };
    let startup = unsafe { SHGetKnownFolderPath(startup_folder, KF_FLAG_DEFAULT, None) }
        .ok()
        .map(|path| PathBuf::from(owned_string(path)));
    let entries = fs::read_dir(&root).map_err(|error| error.to_string())?;
    let mut directories = vec![entries];

    while let Some(entries) = directories.pop() {
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                // Junctions may create loops or leave the Start Menu tree.
                use std::os::windows::fs::MetadataExt;
                if entry
                    .metadata()
                    .is_ok_and(|data| data.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
                    && let Ok(children) = fs::read_dir(&path)
                {
                    directories.push(children);
                }
                continue;
            }
            let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            if !kind.is_file()
                || !matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "lnk" | "url" | "exe" | "appref-ms"
                )
            {
                continue;
            }

            let relative = path.strip_prefix(&root).unwrap_or(&path);
            if !seen_shortcuts.insert(path.clone()) {
                continue;
            }
            let detail = relative
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(|parent| parent.to_string_lossy().into_owned());
            let identity = identities.identify(&path);
            let in_startup = startup
                .as_ref()
                .is_some_and(|folder| path.starts_with(folder));
            let mut app = App::new(desktop_name(&path), detail, Source::Desktop(path));
            app.common_shortcut = common;
            app.startup_shortcut = in_startup;
            if let Some((identity, path)) = identity {
                if let Identity::Packaged(id) = &identity {
                    app.aumid = Some(id.clone());
                }
                app.identity = Some(identity);
                app.executable_display = path;
            }
            apps.push(app);
        }
    }
    Ok(())
}

fn desktop_name(path: &Path) -> String {
    let wide: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    let item: windows::core::Result<IShellItem> =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) };
    let display = item.and_then(|item| display_name(&item, SIGDN_NORMALDISPLAY));
    let stem = path
        .file_stem()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    match display {
        Ok(display)
            if !display.is_empty()
                && !path
                    .file_name()
                    .is_some_and(|name| name == display.as_str()) =>
        {
            display
        }
        _ => stem.into_owned(),
    }
}

fn packaged_apps(
    apps: &mut Vec<App>,
    shortcuts: &mut shortcut::Index,
) -> windows::core::Result<()> {
    let folder: IShellItem =
        unsafe { SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)? };
    let items: IEnumShellItems = unsafe { folder.BindToHandler(None, &BHID_EnumItems)? };
    let mut seen = HashSet::new();
    let mut packages = packages::Index::new();
    let result = loop {
        let mut item = [None];
        if let Err(error) = unsafe { items.Next(&mut item, None) } {
            break Err(error);
        }
        let [Some(item)] = item else { break Ok(()) };
        let Ok(id) = display_name(&item, SIGDN_PARENTRELATIVEPARSING) else {
            continue;
        };
        let Some(identity) = packaged_id(&id) else {
            continue;
        };
        let Ok(name) = display_name(&item, SIGDN_NORMALDISPLAY) else {
            continue;
        };
        if !name.is_empty() && seen.insert(identity.clone()) {
            packages.record(&item, &identity, shortcuts);
            apps.push(App::new(name, None, Source::Packaged(id)));
        }
    };
    packages.apply(apps);
    result
}

fn display_name(item: &IShellItem, form: SIGDN) -> windows::core::Result<String> {
    let name = unsafe { item.GetDisplayName(form)? };
    Ok(owned_string(name).to_string_lossy().into_owned())
}

fn owned_string(pointer: PWSTR) -> OsString {
    unsafe {
        let value = OsString::from_wide(pointer.as_wide());
        CoTaskMemFree(Some(pointer.0.cast()));
        value
    }
}

pub fn launch(source: &Source, name: &str, elevated: bool) -> Result<(), String> {
    let _apartment = Apartment::enter().map_err(|error| {
        render(
            Msg::ShellApartmentUnavailable,
            &[("detail", &error.to_string())],
        )
    })?;
    let result = if elevated {
        match source {
            Source::Desktop(path) => elevate_desktop(path).map_err(|error| error.to_string()),
            Source::Packaged(id) => elevate_packaged(id),
            Source::Steam { .. } => Err(tr(Msg::LaunchElevationUnavailable).into()),
        }
    } else {
        standard_launch::launch(source)
    };
    result.map_err(|error| render(Msg::LaunchError, &[("name", name), ("detail", &error)]))
}

fn elevate_desktop(path: &Path) -> io::Result<()> {
    let panel = shortcut::control_panel_shortcut(path);
    let path = panel.as_deref().unwrap_or(path);
    let file: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut operation = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("runas").as_ptr(),
        lpFile: file.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    execute_shell(&mut operation)
}

fn execute_shell(operation: &mut SHELLEXECUTEINFOW) -> io::Result<()> {
    if unsafe { ShellExecuteExW(operation) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn packaged_item(id: &str) -> windows::core::Result<IShellItem> {
    let virtual_path: Vec<_> = format!(r"shell:AppsFolder\{id}")
        .encode_utf16()
        .chain([0])
        .collect();
    unsafe { SHCreateItemFromParsingName(PCWSTR(virtual_path.as_ptr()), None) }
}

fn elevate_packaged(id: &str) -> Result<(), String> {
    let item = packaged_item(id).map_err(|error| error.to_string())?;
    let pidl = unsafe { SHGetIDListFromObject(&item) }.map_err(|error| error.to_string())?;
    let mut operation = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("runas").as_ptr(),
        lpIDList: pidl.cast(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    let result = execute_shell(&mut operation);
    unsafe { CoTaskMemFree(Some(pidl.cast())) };
    result.map_err(|error| match error.raw_os_error().map(|code| code as u32) {
        Some(ERROR_NO_ASSOCIATION | ERROR_NOT_SUPPORTED) => {
            tr(Msg::LaunchElevationUnavailable).into()
        }
        _ => error.to_string(),
    })
}

fn launch_packaged(id: &str) -> windows::core::Result<()> {
    let id: Vec<_> = id.encode_utf16().chain([0]).collect();
    unsafe {
        let manager: IApplicationActivationManager =
            CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)?;
        let _ = CoAllowSetForegroundWindow(&manager, None);
        manager.ActivateApplication(PCWSTR(id.as_ptr()), PCWSTR::null(), AO_NONE)?;
    }
    Ok(())
}
