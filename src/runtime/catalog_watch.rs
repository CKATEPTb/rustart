use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use windows::{
    ApplicationModel::{
        PackageCatalog, PackageInstallingEventArgs, PackageStatusChangedEventArgs,
        PackageUninstallingEventArgs, PackageUpdatingEventArgs,
    },
    Foundation::TypedEventHandler,
    Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{
            FOLDERID_CommonPrograms, FOLDERID_CommonStartMenu, FOLDERID_Programs,
            FOLDERID_StartMenu, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
        },
    },
    core::GUID,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, HWND, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FindCloseChangeNotification,
        FindFirstChangeNotificationW, FindNextChangeNotification,
    },
    System::SystemServices::MAXIMUM_WAIT_OBJECTS,
    UI::WindowsAndMessaging::PostMessageW,
};

use super::PACKAGE_CHANGED;

pub(super) struct Watch {
    folders: [Option<Folder>; 2],
    pub(super) steam: SteamFolders,
    package: Option<Packages>,
}

// MsgWaitForMultipleObjectsEx also waits for the message queue. Reserve a
// further two handles for the user's and the shared Start menu folders.
const MAX_STEAM_FOLDERS: usize = MAXIMUM_WAIT_OBJECTS as usize - 1 - 2;

pub(super) struct SteamFolders {
    folders: Vec<Folder>,
    pub(super) complete: bool,
}

impl SteamFolders {
    pub(super) fn open(roots: &[PathBuf]) -> Self {
        let folders: Vec<_> = roots
            .iter()
            .take(MAX_STEAM_FOLDERS)
            .filter_map(|root| Folder::open(root, false))
            .collect();
        Self {
            complete: folders.len() == roots.len(),
            folders,
        }
    }
}

impl Watch {
    pub(super) fn new(owner: HWND, signal: Arc<AtomicBool>) -> Self {
        Self {
            // Parent folders include immediate renames/deletion of Programs
            // as well as modification of shortcuts anywhere under it.
            folders: [
                (&FOLDERID_Programs, &FOLDERID_StartMenu),
                (&FOLDERID_CommonPrograms, &FOLDERID_CommonStartMenu),
            ]
            .map(|(programs, menu)| Folder::new(programs, menu)),
            steam: SteamFolders::open(&[]),
            package: Packages::new(owner, signal),
        }
    }

    pub(super) fn desktop_complete(&self) -> bool {
        self.folders.iter().all(Option::is_some)
            && self.package.as_ref().is_some_and(Packages::complete)
    }

    pub(super) fn handles(&self, handles: &mut Vec<HANDLE>) {
        handles.extend(
            self.folders
                .iter()
                .flatten()
                .chain(&self.steam.folders)
                .map(|folder| folder.0),
        );
    }

    pub(super) fn rearm(&mut self, handle: HANDLE) {
        if unsafe { FindNextChangeNotification(handle) } != 0 {
            return;
        }
        for folder in &mut self.folders {
            if folder.as_ref().is_some_and(|folder| folder.0 == handle) {
                *folder = None;
                return;
            }
        }
        self.steam.folders.retain(|folder| folder.0 != handle);
        self.steam.complete = false;
    }

    pub(super) fn stop_folders(&mut self) {
        self.folders = [None, None];
        self.steam.folders.clear();
        self.steam.complete = false;
    }
}

struct Folder(HANDLE);

// Directory change notifications can be opened and waited on by different
// threads. Ownership moves once, from the indexer to the UI's wait loop.
unsafe impl Send for Folder {}

impl Folder {
    fn new(programs: &GUID, menu: &GUID) -> Option<Self> {
        let programs = known_folder(programs)?;
        let menu = known_folder(menu);
        let root = menu
            .filter(|menu| {
                programs
                    .parent()
                    .is_some_and(|parent| parent.as_os_str().eq_ignore_ascii_case(menu.as_os_str()))
            })
            .unwrap_or(programs);
        Self::open(&root, true)
    }

    fn open(root: &Path, recursive: bool) -> Option<Self> {
        let path: Vec<_> = root.as_os_str().encode_wide().chain([0]).collect();
        let handle = unsafe {
            FindFirstChangeNotificationW(
                path.as_ptr(),
                i32::from(recursive),
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE,
            )
        };
        (handle != INVALID_HANDLE_VALUE).then(|| Self(handle))
    }
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
    let result = PathBuf::from(unsafe { OsString::from_wide(path.as_wide()) });
    unsafe { CoTaskMemFree(Some(path.0.cast())) };
    Some(result)
}

impl Drop for Folder {
    fn drop(&mut self) {
        unsafe { FindCloseChangeNotification(self.0) };
    }
}

struct Packages {
    catalog: PackageCatalog,
    installing: Option<i64>,
    uninstalling: Option<i64>,
    updating: Option<i64>,
    status: Option<i64>,
}

impl Packages {
    fn new(owner: HWND, signal: Arc<AtomicBool>) -> Option<Self> {
        let catalog = PackageCatalog::OpenForCurrentUser().ok()?;
        let owner = owner as usize;
        let callback = move || {
            // Deployment can issue several events per package. At most one
            // invalidation needs to be pending in the window's queue.
            if !signal.swap(true, Ordering::AcqRel)
                && unsafe { PostMessageW(owner as HWND, PACKAGE_CHANGED, 0, 0) } == 0
            {
                signal.store(false, Ordering::Release);
            }
        };
        let notify = callback.clone();
        let installing = catalog
            .PackageInstalling(&TypedEventHandler::<
                PackageCatalog,
                PackageInstallingEventArgs,
            >::new(move |_, args| {
                if args
                    .as_ref()
                    .is_none_or(|event| event.IsComplete().unwrap_or(true))
                {
                    notify();
                }
                Ok(())
            }))
            .ok();
        let notify = callback.clone();
        let uninstalling = catalog
            .PackageUninstalling(&TypedEventHandler::<
                PackageCatalog,
                PackageUninstallingEventArgs,
            >::new(move |_, args| {
                if args
                    .as_ref()
                    .is_none_or(|event| event.IsComplete().unwrap_or(true))
                {
                    notify();
                }
                Ok(())
            }))
            .ok();
        let notify = callback.clone();
        let updating = catalog
            .PackageUpdating(
                &TypedEventHandler::<PackageCatalog, PackageUpdatingEventArgs>::new(
                    move |_, args| {
                        if args
                            .as_ref()
                            .is_none_or(|event| event.IsComplete().unwrap_or(true))
                        {
                            notify();
                        }
                        Ok(())
                    },
                ),
            )
            .ok();
        let status = catalog
            .PackageStatusChanged(&TypedEventHandler::<
                PackageCatalog,
                PackageStatusChangedEventArgs,
            >::new(move |_, _| {
                callback();
                Ok(())
            }))
            .ok();
        Some(Self {
            catalog,
            installing,
            uninstalling,
            updating,
            status,
        })
    }

    fn complete(&self) -> bool {
        self.installing.is_some() && self.uninstalling.is_some() && self.updating.is_some()
    }
}

impl Drop for Packages {
    fn drop(&mut self) {
        if let Some(token) = self.installing {
            let _ = self.catalog.RemovePackageInstalling(token);
        }
        if let Some(token) = self.uninstalling {
            let _ = self.catalog.RemovePackageUninstalling(token);
        }
        if let Some(token) = self.updating {
            let _ = self.catalog.RemovePackageUpdating(token);
        }
        if let Some(token) = self.status {
            let _ = self.catalog.RemovePackageStatusChanged(token);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    use windows_sys::Win32::{
        Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::WaitForSingleObject,
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let tick = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("Rustart.Watch.{}.{tick}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn directory_watch_reports_nested_creation_changes_and_deletion() {
        let cleanup = Fixture::new();
        let nested = cleanup.0.join("nested").join("programs");
        fs::create_dir_all(&nested).unwrap();
        let watch = Folder::open(&cleanup.0, true).unwrap();
        let file = nested.join("test.lnk");
        let wait_and_rearm = || {
            assert_eq!(
                unsafe { WaitForSingleObject(watch.0, 2_000) },
                WAIT_OBJECT_0
            );
            assert_ne!(unsafe { FindNextChangeNotification(watch.0) }, 0);
        };
        fs::write(&file, b"created").unwrap();
        wait_and_rearm();
        fs::write(&file, b"updated shortcut").unwrap();
        wait_and_rearm();
        fs::remove_file(file).unwrap();
        wait_and_rearm();
    }

    #[test]
    fn steam_watches_manifests_and_library_configuration_without_watching_game_content() {
        let root = Fixture::new();
        let primary = root.0.join("primary").join("steamapps");
        let secondary = root.0.join("secondary").join("steamapps");
        let game = secondary.join("common").join("game");
        fs::create_dir_all(&primary).unwrap();
        fs::create_dir_all(&game).unwrap();
        let watch = SteamFolders::open(&[primary.clone(), secondary.clone()]);
        assert!(watch.complete);
        fs::write(game.join("game.dat"), b"data").unwrap();
        let secondary_watch = watch.folders[1].0;
        assert_eq!(
            unsafe { WaitForSingleObject(secondary_watch, 50) },
            WAIT_TIMEOUT
        );
        let manifest = secondary.join("appmanifest_42.acf");
        fs::write(&manifest, b"installed").unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(secondary_watch, 2000) },
            WAIT_OBJECT_0
        );
        assert_ne!(unsafe { FindNextChangeNotification(secondary_watch) }, 0);
        fs::remove_file(manifest).unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(secondary_watch, 2000) },
            WAIT_OBJECT_0
        );
        fs::write(primary.join("libraryfolders.vdf"), b"changed").unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(watch.folders[0].0, 2000) },
            WAIT_OBJECT_0
        );
        assert!(SteamFolders::open(&[]).complete);
        assert!(!SteamFolders::open(&[root.0.join("offline")]).complete);
        let overflow = SteamFolders::open(&vec![primary; MAX_STEAM_FOLDERS + 1]);
        assert!(!overflow.complete);
        assert_eq!(overflow.folders.len(), MAX_STEAM_FOLDERS);
    }

    #[test]
    #[ignore]
    fn probe_unpacked_package_catalog_subscription() {
        use std::ptr::null_mut;
        use windows_sys::Win32::{
            System::{
                Com::{
                    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
                    CoUninitialize,
                },
                LibraryLoader::GetModuleHandleW,
            },
            UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE},
        };
        use windows_sys::core::w;
        unsafe {
            assert!(
                CoInitializeEx(
                    std::ptr::null(),
                    (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32
                ) >= 0
            );
            let hwnd = CreateWindowExW(
                0,
                w!("STATIC"),
                w!("Rustart PackageCatalog Probe"),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            );
            assert!(!hwnd.is_null());
            let packages = Packages::new(hwnd, Arc::new(AtomicBool::new(false)));
            let complete = packages.as_ref().is_some_and(Packages::complete);
            let events = packages.as_ref().map(|p| {
                [
                    p.installing.is_some(),
                    p.uninstalling.is_some(),
                    p.updating.is_some(),
                    p.status.is_some(),
                ]
            });
            drop(packages);
            DestroyWindow(hwnd);
            CoUninitialize();
            assert!(
                complete,
                "PackageCatalog activation/subscriptions failed: {events:?}"
            );
            eprintln!("PackageCatalog [install, uninstall, update, status]: {events:?}");
        }
    }
}
