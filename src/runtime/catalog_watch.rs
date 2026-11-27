use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::null_mut,
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
    UI::WindowsAndMessaging::PostMessageW,
};

use super::PACKAGE_CHANGED;

pub(super) struct Watch {
    folders: [Option<Folder>; 2],
    package: Option<Packages>,
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
            package: Packages::new(owner, signal),
        }
    }

    pub(super) fn complete(&self) -> bool {
        self.folders.iter().all(Option::is_some)
            && self.package.as_ref().is_some_and(Packages::complete)
    }

    pub(super) fn handles(&self) -> ([HANDLE; 2], u32) {
        let mut handles = [null_mut(); 2];
        let mut count = 0;
        for folder in self.folders.iter().flatten() {
            handles[count] = folder.0;
            count += 1;
        }
        (handles, count as u32)
    }

    pub(super) fn rearm(&mut self, handle: HANDLE) {
        for folder in &mut self.folders {
            if folder.as_ref().is_some_and(|folder| folder.0 == handle)
                && unsafe { FindNextChangeNotification(handle) } == 0
            {
                *folder = None;
                return;
            }
        }
    }

    pub(super) fn stop_folders(&mut self) {
        self.folders = [None, None];
    }
}

struct Folder(HANDLE);

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
        Self::open(&root)
    }

    fn open(root: &Path) -> Option<Self> {
        let path: Vec<_> = root.as_os_str().encode_wide().chain([0]).collect();
        let handle = unsafe {
            FindFirstChangeNotificationW(
                path.as_ptr(),
                1,
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
    use windows_sys::Win32::{Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject};

    #[test]
    fn directory_watch_reports_nested_creation_changes_and_deletion() {
        let tick = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("Rustart.Watch.{}.{tick}", std::process::id()));
        fs::create_dir(&path).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let cleanup = Cleanup(path);
        let nested = cleanup.0.join("nested").join("programs");
        fs::create_dir_all(&nested).unwrap();
        let watch = Folder::open(&cleanup.0).unwrap();
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
    #[ignore]
    fn probe_unpacked_package_catalog_subscription() {
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
