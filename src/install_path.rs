use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, Prefix},
};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

// The executable handle protects the file. These handles keep its path stable
// while the shell resolves it to show the UAC dialog and start the helper.
pub(crate) fn pin_ancestors(path: &Path) -> io::Result<Vec<File>> {
    if !path.is_absolute()
        || !matches!(path.components().next(),
            Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
    {
        return Err(io::Error::other("The installer needs a local directory"));
    }
    path.ancestors()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|ancestor| {
            let dir = OpenOptions::new()
                .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(ancestor)?;
            let metadata = dir.metadata()?;
            if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(io::Error::other(
                    "Unexpected installer directory or reparse point",
                ));
            }
            Ok(dir)
        })
        .collect()
}
