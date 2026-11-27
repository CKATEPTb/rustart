use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::OsString,
    fs,
    mem::size_of,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::{NonNull, null_mut},
    slice,
};

use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile, STGM_READ,
            STGM_SHARE_DENY_NONE,
        },
        UI::Shell::{
            Common::ITEMIDLIST, FOLDERID_AppsFolder, FOLDERID_System, ILGetSize,
            IShellLinkDataList, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderIDList,
            SHGetKnownFolderPath, SHGetNameFromIDList, SHParseDisplayName,
            SIGDN_PARENTRELATIVEPARSING, SLDF_RUNAS_USER, SLGP_RAWPATH, ShellLink,
        },
    },
    core::{Interface, PCWSTR, w},
};
use windows_sys::Win32::{
    Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FileIdInfo,
        GetDriveTypeW, GetFileInformationByHandleEx,
    },
    System::Environment::ExpandEnvironmentStringsW,
};

use crate::catalog::identity::{Identity, apps_folder_id, executable_path, packaged_id};

use super::owned_string;

struct Pidl(NonNull<ITEMIDLIST>);

impl Pidl {
    fn bytes(&self) -> &[u8] {
        unsafe {
            let pointer = self.0.as_ptr();
            slice::from_raw_parts(pointer.cast(), ILGetSize(Some(pointer)) as usize)
        }
    }
}

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.as_ptr().cast())) };
    }
}

pub(super) struct Index {
    apps_folder: Option<Pidl>,
    explorer_path: Option<String>,
    local_ids: HashMap<PathBuf, Option<LocalFile>>,
}

struct LocalFile {
    path: PathBuf,
    identity: Identity,
}

impl Index {
    pub(super) fn new() -> Self {
        let apps_folder = unsafe { SHGetKnownFolderIDList(&FOLDERID_AppsFolder, 0, None) }
            .ok()
            .and_then(NonNull::new)
            .map(Pidl);
        Self {
            apps_folder,
            explorer_path: std::env::var_os("SystemRoot")
                .and_then(|root| executable_path(&Path::new(&root).join("explorer.exe"))),
            local_ids: HashMap::new(),
        }
    }

    pub(super) fn identify(&mut self, source: &Path) -> Option<(Identity, Option<String>)> {
        if source.extension()?.eq_ignore_ascii_case("exe") {
            return self.binary(source);
        }
        if !source.extension()?.eq_ignore_ascii_case("lnk") {
            return None;
        }

        let link = open(source)?;
        let target = read_target(&link);
        let virtual_id = target
            .as_deref()
            .and_then(|path| apps_folder_id(path.to_str()?));
        if let Some(id) = virtual_id {
            return Some((Identity::Packaged(id), None));
        }
        let Some(target) = target else {
            return self
                .virtual_app(&link)
                .map(|id| (Identity::Packaged(id), None));
        };

        let path = executable_path(&target)?;
        if self.explorer_path.as_deref() == Some(path.as_str())
            && let Some(id) = explorer_app(&link)
        {
            return Some((Identity::Packaged(id), None));
        }
        Some(self.binary_with_path(&target, path))
    }

    fn binary(&mut self, path: &Path) -> Option<(Identity, Option<String>)> {
        let name = executable_path(path)?;
        Some(self.binary_with_path(path, name))
    }

    pub(super) fn package_binary(
        &mut self,
        path: &Path,
        base: &Path,
    ) -> Option<(Identity, String)> {
        executable_path(path)?;
        let local = self.local_file(path)?;
        if !local.path.starts_with(base) {
            return None;
        }
        Some((local.identity.clone(), path.to_string_lossy().into_owned()))
    }

    fn binary_with_path(&mut self, path: &Path, name: String) -> (Identity, Option<String>) {
        let identity = self.local_file(path).map(|file| file.identity.clone());
        (
            identity.unwrap_or(Identity::Executable(name)),
            Some(path.to_string_lossy().into_owned()),
        )
    }

    fn local_file(&mut self, path: &Path) -> Option<&LocalFile> {
        self.local_ids
            .entry(path.into())
            .or_insert_with(|| {
                let path = resolve_local_file(path)?;
                let identity = file_id(&path)?;
                Some(LocalFile {
                    path: path.into_owned(),
                    identity,
                })
            })
            .as_ref()
    }

    fn virtual_app(&self, link: &IShellLinkW) -> Option<String> {
        let folder = self.apps_folder.as_ref()?.bytes();
        let target = Pidl(NonNull::new(unsafe { link.GetIDList().ok()? })?);
        // Inspect the saved PIDL before asking the shell for its name. Only a
        // child of our known local AppsFolder may be bound to a shell provider.
        let child = target
            .bytes()
            .strip_prefix(folder.get(..folder.len().checked_sub(2)?)?)?;
        let child_size = u16::from_le_bytes(child.get(..2)?.try_into().ok()?) as usize;
        if child_size < 2 || child_size + 2 != child.len() {
            return None;
        }
        let id =
            unsafe { SHGetNameFromIDList(target.0.as_ptr(), SIGDN_PARENTRELATIVEPARSING).ok()? };
        packaged_id(owned_string(id).to_str()?)
    }
}

pub(super) fn target(source: &Path) -> Option<PathBuf> {
    read_target(&open(source)?)
}

// The Start menu's Control Panel link points to a virtual root rather than a
// file. Compare its entire saved PIDL with the one Windows gives that root.
fn control_panel_executable(link: &IShellLinkW) -> Option<PathBuf> {
    let target = Pidl(NonNull::new(unsafe { link.GetIDList().ok()? })?);
    let mut panel = null_mut();
    unsafe {
        SHParseDisplayName(
            w!("::{5399E694-6CE5-4D6C-8FCE-1D8870FDCBA0}"),
            None,
            &mut panel,
            0,
            None,
        )
        .ok()?;
    }
    let panel = Pidl(NonNull::new(panel)?);
    if target.bytes() != panel.bytes() {
        return None;
    }
    let system = unsafe { SHGetKnownFolderPath(&FOLDERID_System, KF_FLAG_DEFAULT, None).ok()? };
    Some(PathBuf::from(owned_string(system)).join("control.exe"))
}

pub(super) fn control_panel_shortcut(source: &Path) -> Option<PathBuf> {
    let link = open(source)?;
    if read_target(&link).is_some() {
        return None;
    }
    control_panel_executable(&link)
}

pub(super) enum LaunchTarget {
    Desktop {
        path: PathBuf,
        arguments: OsString,
        directory: Option<PathBuf>,
        show: Option<u16>,
    },
    Package(String),
    RequiresElevation,
}

pub(super) fn for_standard_launch(source: &Path) -> Option<LaunchTarget> {
    let link = open(source)?;
    let data: IShellLinkDataList = link.cast().ok()?;
    if unsafe { data.GetFlags().ok()? } & SLDF_RUNAS_USER.0 as u32 != 0 {
        return Some(LaunchTarget::RequiresElevation);
    }
    let Some(path) = read_target(&link) else {
        if let Some(path) = control_panel_executable(&link) {
            return Some(LaunchTarget::Desktop {
                path,
                arguments: OsString::new(),
                directory: None,
                show: None,
            });
        }
        return Index::new().virtual_app(&link).map(LaunchTarget::Package);
    };
    if let Some(id) = path.to_str().and_then(apps_folder_id) {
        return Some(LaunchTarget::Package(id));
    }
    if path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"))
        && super::privilege::explorer_image()
            .is_ok_and(|explorer| super::privilege::paths_equal(&path, &explorer))
        && let Some(id) = explorer_app(&link)
    {
        return Some(LaunchTarget::Package(id));
    }
    executable_path(&path)?;

    let mut buffer = vec![0; 32768];
    unsafe { link.GetArguments(&mut buffer).ok()? };
    let arguments = OsString::from_wide(nul_terminated(&buffer)?);
    buffer.fill(0);
    unsafe { link.GetWorkingDirectory(&mut buffer).ok()? };
    let directory = if nul_terminated(&buffer)?.is_empty() {
        None
    } else {
        let directory = expand_path(&buffer)?;
        if !directory.is_absolute() {
            return None;
        }
        Some(directory)
    };
    Some(LaunchTarget::Desktop {
        path,
        arguments,
        directory,
        show: unsafe { link.GetShowCmd().ok() }
            .and_then(|show| u16::try_from(show.0).ok())
            .filter(|&show| show <= 11),
    })
}

fn open(source: &Path) -> Option<IShellLinkW> {
    if !source.is_absolute() || !source.extension()?.eq_ignore_ascii_case("lnk") {
        return None;
    }
    let source: Vec<_> = source.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        file.Load(PCWSTR(source.as_ptr()), STGM_READ | STGM_SHARE_DENY_NONE)
            .ok()?;
        Some(link)
    }
}

fn explorer_app(link: &IShellLinkW) -> Option<String> {
    let mut buffer = vec![0; 32768];
    unsafe { link.GetArguments(&mut buffer).ok()? };
    apps_folder_id(&String::from_utf16_lossy(nul_terminated(&buffer)?))
}

fn read_target(link: &IShellLinkW) -> Option<PathBuf> {
    // Read only the persisted target. Resolve could search drives, retry over
    // the network or ask the user to repair a shortcut.
    let mut buffer = vec![0; 32768];
    unsafe {
        link.GetPath(&mut buffer, null_mut(), SLGP_RAWPATH.0 as u32)
            .ok()?
    };
    expand_path(&buffer)
}

fn expand_path(buffer: &[u16]) -> Option<PathBuf> {
    let raw = nul_terminated(buffer)?;
    if raw.is_empty() {
        return None;
    }
    if !raw.contains(&(b'%' as u16)) {
        return Some(OsString::from_wide(raw).into());
    }
    let required = unsafe { ExpandEnvironmentStringsW(buffer.as_ptr(), null_mut(), 0) };
    if required == 0 || required > 32768 {
        return None;
    }
    let mut expanded = vec![0; required as usize];
    let length =
        unsafe { ExpandEnvironmentStringsW(buffer.as_ptr(), expanded.as_mut_ptr(), required) };
    Some(OsString::from_wide(expanded.get(..length.checked_sub(1)? as usize)?).into())
}

fn nul_terminated(buffer: &[u16]) -> Option<&[u16]> {
    buffer.get(..buffer.iter().position(|&unit| unit == 0)?)
}

fn file_id(path: &Path) -> Option<Identity> {
    let handle = fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    let mut id = FILE_ID_INFO::default();
    (unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileIdInfo,
            (&raw mut id).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } != 0)
        .then_some(Identity::LocalFile(
            id.VolumeSerialNumber,
            id.FileId.Identifier,
        ))
}

pub(super) fn resolve_local_file(original: &Path) -> Option<Cow<'_, Path>> {
    const MAX_REDIRECTS: usize = 16;
    let mut path = Cow::Borrowed(original);
    let mut seen = Vec::new();
    for _ in 0..=MAX_REDIRECTS {
        if !fixed_drive(&path) {
            return None;
        }
        if path.components().any(|part| part == Component::ParentDir) {
            path = Cow::Owned(collapse_parents(&path)?);
        }
        let LocalPath::Redirect(next) = inspect_local_path(&path)? else {
            return Some(path);
        };
        if next == original || seen.contains(&next) {
            return None;
        }
        seen.push(next.clone());
        path = Cow::Owned(next);
    }
    None
}

fn fixed_drive(path: &Path) -> bool {
    const DRIVE_FIXED: u32 = 3;
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let fixed_candidate = match prefix.kind() {
        Prefix::Disk(_) | Prefix::VerbatimDisk(_) => true,
        Prefix::Verbatim(volume) => volume
            .to_str()
            .and_then(|volume| volume.get(..7))
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("volume{")),
        _ => false,
    };
    if !fixed_candidate || !path.is_absolute() {
        return false;
    }
    let Some(root) = path.ancestors().last() else {
        return false;
    };
    let root: Vec<_> = root.as_os_str().encode_wide().chain([0]).collect();
    unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_FIXED }
}

enum LocalPath {
    File,
    Redirect(PathBuf),
}

fn inspect_local_path(path: &Path) -> Option<LocalPath> {
    let ancestors: Vec<_> = path.ancestors().collect();
    for ancestor in ancestors.into_iter().rev() {
        let metadata = fs::symlink_metadata(ancestor).ok()?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            // read_link extracts the stored target of a symlink or junction. Other
            // reparse tags fail closed without probing their destinations.
            let target = fs::read_link(ancestor).ok()?;
            return redirect(path, ancestor, target).map(LocalPath::Redirect);
        }
        if ancestor == path {
            return metadata.is_file().then_some(LocalPath::File);
        }
    }
    None
}

fn redirect(path: &Path, link: &Path, mut target: PathBuf) -> Option<PathBuf> {
    if !target.is_absolute() {
        if target.has_root() || matches!(target.components().next(), Some(Component::Prefix(_))) {
            return None;
        }
        target = link.parent()?.join(target);
    }
    let suffix = path.strip_prefix(link).ok()?;
    if !suffix.as_os_str().is_empty() {
        target.push(suffix);
    }
    Some(target)
}

fn collapse_parents(path: &Path) -> Option<PathBuf> {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            Component::CurDir => (),
            _ => result.push(part.as_os_str()),
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_only_the_control_panel_namespace_link_as_a_standard_process() {
        use windows::{Win32::UI::Shell::SHParseDisplayName, core::w};
        use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

        let _apartment = super::super::Apartment::enter().unwrap();
        let fixture =
            std::env::temp_dir().join(format!("rustart-control-panel-{}.lnk", std::process::id()));
        struct Cleanup<'a>(&'a Path);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = fs::remove_file(self.0);
            }
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&fixture)
            .unwrap();
        let _cleanup = Cleanup(&fixture);
        let fixture_wide: Vec<_> = fixture.as_os_str().encode_wide().chain([0]).collect();
        let save_link = |target, run_as| unsafe {
            let mut id = null_mut();
            SHParseDisplayName(target, None, &mut id, 0, None).unwrap();
            let id = Pidl(NonNull::new(id).unwrap());
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.SetIDList(id.0.as_ptr()).unwrap();
            link.SetArguments(w!("dummy-argument")).unwrap();
            if run_as {
                let data: IShellLinkDataList = link.cast().unwrap();
                data.SetFlags(data.GetFlags().unwrap() | SLDF_RUNAS_USER.0 as u32)
                    .unwrap();
            }
            let file: IPersistFile = link.cast().unwrap();
            file.Save(PCWSTR(fixture_wide.as_ptr()), true).unwrap();
        };

        let control_panel = w!("::{5399E694-6CE5-4D6C-8FCE-1D8870FDCBA0}");
        save_link(control_panel, false);
        let Some(LaunchTarget::Desktop {
            path, arguments, ..
        }) = for_standard_launch(&fixture)
        else {
            panic!("the Control Panel shortcut did not resolve to a process");
        };
        let mut system = [0; 32768];
        let length = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) };
        assert!(length > 0 && (length as usize) < system.len());
        let system = PathBuf::from(OsString::from_wide(&system[..length as usize]));
        assert_eq!(path, system.join("control.exe"));
        assert!(arguments.is_empty());
        assert_eq!(control_panel_shortcut(&fixture), Some(path));

        save_link(w!("::{645FF040-5081-101B-9F08-00AA002F954E}"), false);
        assert!(for_standard_launch(&fixture).is_none());
        assert!(control_panel_shortcut(&fixture).is_none());

        save_link(control_panel, true);
        assert!(matches!(
            for_standard_launch(&fixture),
            Some(LaunchTarget::RequiresElevation)
        ));
    }

    fn local_file_id(path: &Path) -> Option<Identity> {
        file_id(&resolve_local_file(path)?)
    }

    #[test]
    fn matches_hard_links_to_the_same_local_file() {
        let exe = std::env::current_exe().unwrap();
        let base = format!("rustart-identity-{}", std::process::id());
        let path = exe.with_file_name(format!("{base}.exe"));
        let target = exe.with_file_name(format!("{base}-alias.exe"));
        fs::write(&path, b"test").unwrap();
        fs::hard_link(&path, &target).unwrap();
        let original = local_file_id(&path);
        let alias = local_file_id(&target);
        fs::remove_file(&target).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(alias.is_some());
        assert_eq!(alias, original);
    }

    #[test]
    fn resolves_a_relative_link_target_and_rejects_remote_drives() {
        let file = Path::new(r"C:\Start\Link\Tool.exe");
        let link = Path::new(r"C:\Start\Link");
        let path = redirect(file, link, PathBuf::from(r"..\Apps")).unwrap();
        assert_eq!(
            collapse_parents(&path).unwrap(),
            Path::new(r"C:\Apps\Tool.exe")
        );
        let remote = redirect(file, link, PathBuf::from(r"\\server\share\Apps")).unwrap();
        assert!(!fixed_drive(&remote));
        assert!(!fixed_drive(Path::new(r"\\?\UNC\server\share\Tool.exe")));
        assert!(redirect(file, link, PathBuf::from(r"C:relative")).is_none());
    }

    #[test]
    fn matches_files_through_local_directory_links_without_elevation() {
        use std::{
            os::windows::{fs::symlink_dir, process::CommandExt},
            process::Command,
        };
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        let fixture = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("rustart-directory-link-{}", std::process::id()));
        let directory = fixture.join("Apps");
        let link = fixture.join("Alias");
        let file = directory.join("Tool.exe");

        struct Cleanup<'a> {
            root: &'a Path,
            directory: &'a Path,
            link: &'a Path,
            file: &'a Path,
        }
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = fs::remove_dir(self.link);
                let _ = fs::remove_file(self.file);
                let _ = fs::remove_dir(self.directory);
                let _ = fs::remove_dir(self.root);
            }
        }

        fs::create_dir(&fixture).unwrap();
        let _cleanup = Cleanup {
            root: &fixture,
            directory: &directory,
            link: &link,
            file: &file,
        };
        fs::create_dir(&directory).unwrap();
        fs::write(&file, b"test").unwrap();
        let symbolic = match symlink_dir("Apps", &link) {
            Ok(()) => true,
            Err(error) if matches!(error.raw_os_error(), Some(5 | 1314)) => {
                // Windows can create a directory junction without the symbolic
                // link privilege. Use only this temporary test fixture.
                let created = Command::new("powershell.exe")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "New-Item -ItemType Junction -Path $env:RUSTART_TEST_LINK -Target $env:RUSTART_TEST_LINK_TARGET -ErrorAction Stop | Out-Null",
                    ])
                    .env("RUSTART_TEST_LINK", &link)
                    .env("RUSTART_TEST_LINK_TARGET", &directory)
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();
                if !created.is_ok_and(|output| output.status.success()) {
                    eprintln!("Local directory link setup unavailable on this machine.");
                    return;
                }
                false
            }
            Err(error) => panic!("Could not create local fixture symlink: {error}"),
        };
        let identity = local_file_id(&file);
        assert!(identity.is_some());
        assert_eq!(identity, local_file_id(&link.join("Tool.exe")));
        if symbolic {
            fs::remove_dir(&link).unwrap();
            symlink_dir("Alias", &link).unwrap();
            assert!(local_file_id(&link.join("Tool.exe")).is_none());
        }
    }
}
