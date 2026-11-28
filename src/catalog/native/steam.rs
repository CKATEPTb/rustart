mod client;
mod vdf;

use crate::catalog::{App, Source};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, File},
    io::Read,
    os::windows::ffi::OsStrExt,
    path::{Component, Path, PathBuf, Prefix},
};
use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

pub(super) use client::executable;
pub(crate) use client::steam_icon;

pub(super) fn app_id(text: &str) -> Option<u32> {
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|id| *id != 0)
}

fn read_vdf(path: &Path) -> Option<vdf::Object> {
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    let file = File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_BYTES + 1).read_to_string(&mut text).ok()?;
    if text.len() as u64 > MAX_BYTES || text.contains('\0') {
        return None;
    }
    vdf::parse(&text)
}

fn local_library(path: &Path) -> bool {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    if !path.is_absolute() || !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
        return false;
    }
    let Some(root) = path.ancestors().last() else {
        return false;
    };
    let root: Vec<_> = root.as_os_str().encode_wide().chain([0]).collect();
    // Keep a disconnected library's drive letter so the next search can
    // retry its watch. Remote drives and UNC paths are excluded.
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_RAMDISK: u32 = 6;
    matches!(
        unsafe { GetDriveTypeW(root.as_ptr()) },
        DRIVE_NO_ROOT_DIR | DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_RAMDISK
    )
}

fn library_folders(root: &Path) -> Vec<PathBuf> {
    if !local_library(root) {
        return Vec::new();
    }
    let steamapps = |root: &Path| root.components().collect::<PathBuf>().join("steamapps");
    let primary = steamapps(root);
    let mut config_file = primary.join("libraryfolders.vdf");
    if !config_file.is_file() {
        config_file = root.join("config").join("libraryfolders.vdf");
    }
    let config = read_vdf(&config_file);
    let mut folders = vec![primary];
    let Some(entries) = config.as_ref().and_then(|doc| doc.object("LibraryFolders")) else {
        return folders;
    };
    let mut seen = HashSet::from([folders[0].to_string_lossy().to_lowercase()]);
    for (key, value) in entries.iter() {
        if key.parse::<u32>().is_err() {
            continue;
        }
        let Some(path) = value.as_text().or_else(|| value.as_object()?.text("path")) else {
            continue;
        };
        let root = Path::new(path);
        if local_library(root) {
            let folder = steamapps(root);
            if seen.insert(folder.to_string_lossy().to_lowercase()) {
                folders.push(folder);
            }
        }
    }
    folders
}

pub(crate) fn steam_library_folders() -> Vec<PathBuf> {
    executable()
        .as_deref()
        .and_then(Path::parent)
        .map(library_folders)
        .unwrap_or_default()
}

pub(super) fn launch_target(app_id: u32) -> Option<(PathBuf, OsString)> {
    if app_id == 0 {
        return None;
    }
    Some((executable()?, format!("-applaunch {app_id}").into()))
}

pub(super) fn discover(apps: &mut Vec<App>) {
    discover_in(&steam_library_folders(), apps);
}

fn discover_in(folders: &[PathBuf], apps: &mut Vec<App>) {
    let mut seen = HashSet::new();
    for folder in folders {
        let Ok(entries) = fs::read_dir(folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let filename = entry.file_name();
            let Some(name) = filename.to_str() else {
                continue;
            };
            let name = name.to_ascii_lowercase();
            let Some(app_id) = name
                .strip_prefix("appmanifest_")
                .and_then(|name| name.strip_suffix(".acf"))
                .and_then(app_id)
            else {
                continue;
            };
            // Steamworks Common Redistributables is a shared runtime.
            if app_id == 228980 || seen.contains(&app_id) {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_file())
                && let Some(app) = installed_app(folder, &entry.path(), app_id)
            {
                seen.insert(app_id);
                apps.push(app);
            }
        }
    }
}

fn installed_app(folder: &Path, manifest: &Path, expected_id: u32) -> Option<App> {
    let doc = read_vdf(manifest)?;
    let state = doc.object("AppState")?;
    let app_id = app_id(state.text("appid")?)?;
    if app_id != expected_id {
        return None;
    }
    if let Some(flags) = state.text("StateFlags") {
        let flags = flags.parse::<u32>().ok()?;
        const FULLY_INSTALLED: u32 = 4;
        const UNINSTALLED: u32 = 1;
        const UNINSTALLING: u32 = 2048;
        if flags & FULLY_INSTALLED == 0 || flags & (UNINSTALLED | UNINSTALLING) != 0 {
            return None;
        }
    }
    let name = state.text("name")?.trim();
    let directory = Path::new(state.text("installdir")?);
    let relative = !directory.as_os_str().is_empty()
        && directory.components().all(|part| match part {
            Component::Normal(segment) => segment
                .to_str()
                .is_some_and(|name| !name.ends_with(['.', ' ']) && !name.contains(':')),
            _ => false,
        });
    if name.is_empty() || name.chars().any(char::is_control) || !relative {
        return None;
    }
    let install_dir = folder.join("common").join(directory);
    if !install_dir.is_dir() {
        return None;
    }
    Some(App::new(
        name.into(),
        Some("Steam".into()),
        Source::Steam {
            app_id,
            install_dir,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn scans_installed_games_across_libraries_and_ignores_stale_or_incomplete_manifests() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let tick = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp =
            std::env::temp_dir().join(format!("Rustart.Steam.{}.{tick}", std::process::id()));
        fs::create_dir(&temp).unwrap();
        let fixture = Fixture(temp);
        let root = &fixture.0;
        let second = root.join("extra");
        let primary = root.join("steamapps");
        let external = second.join("steamapps");
        fs::create_dir_all(primary.join("common").join("First")).unwrap();
        fs::create_dir_all(external.join("common").join("Second")).unwrap();
        let libraries = format!(
            r#"LibraryFolders {{ 0 {{ path "{}" }} 1 {{ path "{}" }} 2 "{}" }}"#,
            root.display(),
            second.display(),
            second.display()
        );
        fs::write(primary.join("libraryfolders.vdf"), &libraries).unwrap();
        let folders = library_folders(root);
        assert_eq!(folders, [primary.clone(), external.clone()]);
        let manifest = |folder: &Path, filename_id, id, flags, dir, title| {
            let state = format!(
                r#"AppState {{ appid "{id}" name "{title}" StateFlags "{flags}" installdir "{dir}" }}"#
            );
            fs::write(folder.join(format!("appmanifest_{filename_id}.acf")), state).unwrap();
        };
        manifest(&primary, 42, 42, 4, "First", "First Game");
        manifest(&external, 42, 42, 4, "Second", "Duplicate");
        manifest(&external, 47, 47, 6, "Second", "Игра");
        manifest(&external, 50, 50, 2, "Second", "Staging");
        manifest(&external, 51, 51, 4, "Missing", "Stale");
        manifest(&external, 52, 52, 4, "..", "Escape");
        manifest(&external, 53, 54, 4, "Second", "Wrong ID");
        manifest(&external, 55, 55, 2052, "Second", "Uninstalling");
        manifest(
            &external,
            228980,
            228980,
            4,
            "Second",
            "Steamworks Common Redistributables",
        );
        fs::write(primary.join("appmanifest_99.acf"), "AppState {").unwrap();
        let mut apps = Vec::new();
        discover_in(&folders, &mut apps);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "First Game");
        assert_eq!(
            apps[1].source,
            Source::Steam {
                app_id: 47,
                install_dir: external.join("common").join("Second")
            }
        );
        assert_eq!(crate::catalog::search(&apps, "steam").len(), 2);
        fs::create_dir(root.join("config")).unwrap();
        fs::rename(
            primary.join("libraryfolders.vdf"),
            root.join("config").join("libraryfolders.vdf"),
        )
        .unwrap();
        assert_eq!(library_folders(root), folders);
    }

    #[test]
    fn app_ids_are_positive_decimal_numbers() {
        assert_eq!(app_id("4294967295"), Some(u32::MAX));
        for invalid in ["", "0", "+7", "-7", "7 ", "7/evil", "٤", "4294967296"] {
            assert_eq!(app_id(invalid), None, "{invalid}");
        }
    }
}
