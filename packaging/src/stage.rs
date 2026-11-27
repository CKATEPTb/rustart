//! Scratch directory for a single installer invocation. Keep the directory
//! and its ancestors at stable names while the child may be waiting for UAC.

use std::{
    cell::Cell,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::{
    Win32::{
        Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree},
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::{
            Com::{CoCreateGuid, CoTaskMemFree},
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
        UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath},
    },
    core::{GUID, w},
};

fn check_hr(result: i32) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::other(format!(
            "Windows returned HRESULT {result:#x}"
        )))
    } else {
        Ok(())
    }
}

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

pub(crate) fn program_files() -> io::Result<PathBuf> {
    let mut path = null_mut();
    check_hr(unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, null_mut(), &mut path) })?;
    unsafe {
        let mut len = 0;
        while *path.add(len) != 0 {
            len += 1;
        }
        let result = OsString::from_wide(std::slice::from_raw_parts(path, len));
        CoTaskMemFree(path.cast());
        Ok(result.into())
    }
}

fn unique_directory(base: &Path) -> io::Result<PathBuf> {
    let mut id = GUID::default();
    check_hr(unsafe { CoCreateGuid(&mut id) })?;
    Ok(base.join(format!(
        "Rustart-{:08x}{:04x}{:04x}{:016x}",
        id.data1,
        id.data2,
        id.data3,
        u64::from_be_bytes(id.data4)
    )))
}

struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn administrators_only() -> io::Result<Self> {
        let mut sd = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("O:BAG:BAD:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)"),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(sd))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }

    fn check_directory(directory: &File) -> io::Result<()> {
        // CreateDirectoryW has no handle result. Confirm that the directory
        // we subsequently pinned still has our administrator owner and DACL.
        let (mut owner, mut acl, mut sd) = (null_mut(), null_mut(), null_mut());
        let result = unsafe {
            GetSecurityInfo(
                directory.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut sd,
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let actual = Descriptor(sd);
        let (mut control, mut revision) = (0, 0);
        unsafe {
            if GetSecurityDescriptorControl(actual.0, &mut control, &mut revision) == 0 {
                return Err(io::Error::last_os_error());
            }
            if !owner.is_null()
                && IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0
                && control & SE_DACL_PROTECTED != 0
                && !acl.is_null()
                && IsValidAcl(acl) != 0
                && (0..(*acl).AceCount).all(|index| {
                    let mut ptr = null_mut();
                    if GetAce(acl, index.into(), &mut ptr) == 0
                        || (*ptr.cast::<ACE_HEADER>()).AceType != 0
                    {
                        return false; // We created only ACCESS_ALLOWED_ACEs.
                    }
                    let sid = (&raw mut (*ptr.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast();
                    IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
                        || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                })
            {
                return Ok(());
            }
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unexpected installer directory permissions",
        ))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

fn pin_directory(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(
            "Unexpected installer directory or reparse point",
        ));
    }
    Ok(file)
}

pub(crate) struct Stage {
    exe: PathBuf,
    directory: Option<File>,
    _parents: Vec<File>,
    descriptor: Option<Descriptor>,
    file_created: Cell<bool>,
}

impl Stage {
    pub(crate) fn new() -> io::Result<Self> {
        let (base, descriptor) = if elevated()? {
            (program_files()?, Some(Descriptor::administrators_only()?))
        } else {
            (fs::canonicalize(std::env::temp_dir())?, None)
        };
        // A handle on rustart.exe alone cannot prevent an ancestor from being
        // renamed while Windows is displaying the consent prompt for that path.
        let parents = crate::install_path::pin_ancestors(&base)?;
        let directory = unique_directory(&base)?;
        let attributes = descriptor.as_ref().map(Descriptor::attributes);
        if unsafe {
            CreateDirectoryW(
                wide(&directory).as_ptr(),
                attributes.as_ref().map_or(null(), std::ptr::from_ref),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let guard = pin_directory(&directory).inspect_err(|_| {
            let _ = fs::remove_dir(&directory);
        })?;
        let stage = Self {
            exe: directory.join("rustart.exe"),
            directory: Some(guard),
            _parents: parents,
            descriptor,
            file_created: Cell::new(false),
        };
        if stage.descriptor.is_some() {
            Descriptor::check_directory(stage.directory.as_ref().unwrap())?;
        }
        Ok(stage)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.exe
    }

    pub(crate) fn create_file(&self) -> io::Result<File> {
        let attributes = self.descriptor.as_ref().map(Descriptor::attributes);
        let handle = unsafe {
            CreateFileW(
                wide(&self.exe).as_ptr(),
                GENERIC_WRITE,
                0,
                attributes.as_ref().map_or(null(), std::ptr::from_ref),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        self.file_created.set(true);
        Ok(unsafe { File::from_raw_handle(handle) })
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if self.file_created.get() {
            let _ = fs::remove_file(&self.exe);
        }
        // The package and child keep the directory pinned while UAC resolves
        // its contents. Once both have finished, remove the empty directory.
        drop(self.directory.take());
        let _ = fs::remove_dir(self.exe.parent().unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn stage_is_unique_stays_at_its_name_and_cleans_up_partial_output() {
        let stage = Stage::new().unwrap();
        let other = Stage::new().unwrap();
        assert_ne!(stage.path(), other.path());
        let directory = stage.path().parent().unwrap().to_owned();
        let mut file = stage.create_file().unwrap();
        file.write_all(b"partial extraction").unwrap();
        assert!(stage.create_file().is_err());
        drop(file);
        assert_eq!(fs::read(stage.path()).unwrap(), b"partial extraction");
        assert!(fs::rename(&directory, directory.with_extension("moved")).is_err());
        drop(stage);
        assert!(!directory.exists());
        assert!(other.path().parent().unwrap().is_dir());
    }
}
