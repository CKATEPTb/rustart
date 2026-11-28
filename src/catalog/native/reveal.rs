use std::{
    ffi::{OsStr, OsString},
    fs,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf, Prefix},
};

use windows::{
    ApplicationModel::AppInfo,
    Win32::{
        System::WinRT::{RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize},
        UI::Shell::{IShellItem, SHCreateItemFromParsingName},
    },
    core::{HSTRING, PCWSTR},
};

use super::{Apartment, explorer::StandardDesktop, packaged_item};
use crate::{
    catalog::Source,
    i18n::{Msg, render},
};

pub fn reveal(source: &Source, name: &str) -> Result<(), String> {
    let _apartment = Apartment::enter().map_err(|error| {
        render(
            Msg::ShellApartmentUnavailable,
            &[("detail", &error.to_string())],
        )
    })?;
    super::standard_launch::reveal(source)
        .map_err(|error| render(Msg::RevealError, &[("name", name), ("detail", &error)]))
}

pub(super) fn reveal_standard(
    source: &Source,
    desktop: &StandardDesktop,
) -> windows::core::Result<()> {
    match source {
        Source::Desktop(source) => reveal_desktop(source, desktop),
        Source::Packaged(id) => reveal_packaged(id, desktop),
        Source::Steam { install_dir, .. } => desktop.explore(install_dir),
    }
}

fn reveal_desktop(source: &Path, desktop: &StandardDesktop) -> windows::core::Result<()> {
    if source.is_dir() {
        return desktop.explore(source);
    }
    if source
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"))
        && let Some(target) = super::shortcut::target(source)
        && filesystem_path(&target)
        && let Ok(metadata) = fs::metadata(&target)
    {
        let result = if metadata.is_dir() {
            Some(desktop.explore(&target))
        } else if metadata.is_file() {
            Some(select_file(desktop, &target))
        } else {
            None
        };
        if result.is_some_and(|result| result.is_ok()) {
            return Ok(());
        }
    }
    select_file(desktop, source)
}
fn filesystem_path(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(_)
            | Prefix::VerbatimDisk(_)
            | Prefix::UNC(..)
            | Prefix::VerbatimUNC(..) => true,
            Prefix::Verbatim(volume) => volume
                .to_str()
                .and_then(|text| text.get(..7))
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("volume{")),
            Prefix::DeviceNS(_) => false,
        },
        _ => false,
    }
}
fn select_file(desktop: &StandardDesktop, path: &Path) -> windows::core::Result<()> {
    let wide: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    let _: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)? };
    desktop.select(path.as_os_str())
}

fn reveal_packaged(id: &str, desktop: &StandardDesktop) -> windows::core::Result<()> {
    if let Some(folder) = installed_folder(id)
        && desktop.explore(&folder).is_ok()
    {
        return Ok(());
    }
    // WindowsApps can deny directory enumeration to Explorer. In that case
    // select this application's exact shell item in the AppsFolder.
    packaged_item(id)?;
    desktop.select(OsStr::new(&format!(r"shell:AppsFolder\{id}")))
}

fn installed_folder(id: &str) -> Option<PathBuf> {
    // Resolve the application's registration, including its actual package version
    // and architecture, instead of choosing an arbitrary member of the family.
    let installed = unsafe {
        RoInitialize(RO_INIT_SINGLETHREADED).ok()?;
        let result = AppInfo::GetFromAppUserModelId(&HSTRING::from(id))
            .and_then(|app| app.Package())
            .and_then(|package| package.InstalledPath());
        RoUninitialize();
        result
    };
    let folder = PathBuf::from(OsString::from_wide(&installed.ok()?));
    if !filesystem_path(&folder) {
        return None;
    }
    fs::read_dir(&folder).ok()?;
    Some(folder)
}
#[cfg(test)]
mod tests {
    use super::*;
    use windows::{
        Win32::{
            System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile},
            UI::Shell::{IShellLinkW, ShellLink},
        },
        core::Interface,
    };

    #[test]
    fn accepts_file_paths_on_local_and_remote_storage() {
        for path in [
            r"C:\Program Files\Пример\App.exe",
            r"X:\Remote Apps\App.exe",
            r"C:\Mounted volume\App.exe",
            r"\\Server\Share\App.exe",
            r"\\?\UNC\Server\Share\App.exe",
            r"\\?\Volume{1f19db76-8e8d-452a-b69f-fd97c1573223}\Apps\Demo.exe",
        ] {
            assert!(filesystem_path(Path::new(path)), "{path}");
        }
        for path in [
            r"C:App.exe",
            r"\\.\PhysicalDrive0",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\App.exe",
            "shell:AppsFolder",
            "https://example.com",
        ] {
            assert!(!filesystem_path(Path::new(path)), "{path}");
        }
    }
    #[test]
    fn missing_package_metadata_falls_back_to_exact_apps_folder_item() {
        for id in [
            "",
            "Demo!",
            "!App",
            "Rustart.Nonexistent.Package_0000000000000!App",
        ] {
            assert!(installed_folder(id).is_none());
        }
    }

    #[test]
    fn reads_a_saved_unicode_shortcut_target_without_opening_it() {
        let executable = std::env::current_exe().unwrap();
        let folder = executable.parent().unwrap();
        let fixture = format!("rustart-reveal-{} Пример", std::process::id());
        let target = folder.join(format!("{fixture}.exe"));
        let shortcut = folder.join(format!("{fixture}.lnk"));
        let alternate = folder.join(format!("{fixture} Alternate.lnk"));
        let url_argument = folder.join(format!("{fixture} URL Arg.lnk"));
        let website = folder.join(format!("{fixture} Site.url"));

        struct Cleanup<'a>([&'a Path; 5]);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                for path in self.0 {
                    let _ = fs::remove_file(path);
                }
            }
        }

        let _cleanup = Cleanup([&target, &shortcut, &alternate, &url_argument, &website]);
        let _apartment = Apartment::enter().unwrap();
        fs::write(&target, b"reveal test fixture").unwrap();
        fs::write(
            &website,
            "[InternetShortcut]\r\nURL=https://example.test\r\n",
        )
        .unwrap();
        let target_path: Vec<_> = target.as_os_str().encode_wide().chain([0]).collect();
        for (file, args) in [
            (&shortcut, "--editor"),
            (&alternate, "--viewer"),
            (&url_argument, "--server=https://example.test --app-id=one"),
        ] {
            let shortcut_path: Vec<_> = file.as_os_str().encode_wide().chain([0]).collect();
            let args: Vec<_> = args.encode_utf16().chain([0]).collect();
            unsafe {
                let link: IShellLinkW =
                    CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
                link.SetPath(PCWSTR(target_path.as_ptr())).unwrap();
                link.SetArguments(PCWSTR(args.as_ptr())).unwrap();
                let file: IPersistFile = link.cast().unwrap();
                file.Save(PCWSTR(shortcut_path.as_ptr()), true).unwrap();
            }
        }
        assert_eq!(
            super::super::shortcut::target(&shortcut).as_deref(),
            Some(target.as_path())
        );
        let mut index = super::super::shortcut::Index::new();
        let first = index.identify(&shortcut);
        assert!(first.is_some());
        assert_eq!(first, index.identify(&alternate));
        assert_eq!(first, index.identify(&url_argument));
        assert!(index.identify(&website).is_none());
    }
}
