use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
};

use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

const INVALID: &str = "The Rustart installation fixture requires a private temporary directory.";

fn metadata(path: &Path, directory: bool) -> io::Result<fs::Metadata> {
    let info = fs::symlink_metadata(path)?;
    if info.is_dir() != directory || info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(INVALID));
    }
    Ok(info)
}

fn directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        result => result?,
    }
    metadata(path, true).map(|_| ())
}

pub(super) fn destination(source: &Path, origin: Option<&Path>) -> io::Result<Option<PathBuf>> {
    let Some(program_files) = env::var_os("RUSTART_FIXTURE_PROGRAM_FILES").map(PathBuf::from)
    else {
        return Ok(None);
    };
    let root = program_files
        .parent()
        .ok_or_else(|| io::Error::other(INVALID))?;
    let name = root.file_name().and_then(|name| name.to_str());
    let temp = fs::canonicalize(env::temp_dir())?;
    if !program_files.is_absolute()
        || program_files.file_name() != Some("Program Files".as_ref())
        || !name
            .and_then(|name| name.strip_prefix("rustart-install-test-"))
            .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(io::Error::other(INVALID));
    }
    metadata(root, true)?;
    metadata(&program_files, true)?;
    let canonical = fs::canonicalize(&program_files)?;
    let expected_root = temp.join(root.file_name().unwrap());
    let source = fs::canonicalize(source)?;
    let belongs = source.starts_with(&expected_root)
        || (source.starts_with(&temp)
            && origin
                .and_then(|path| fs::canonicalize(path).ok())
                .is_some_and(|path| path.starts_with(&expected_root)));
    if canonical != expected_root.join("Program Files") || !belongs {
        return Err(io::Error::other(INVALID));
    }
    Ok(Some(
        canonical
            .join("CKATEPTb")
            .join("Rustart")
            .join("rustart.exe"),
    ))
}

pub(super) fn matches(source: &Path, destination: &Path) -> io::Result<bool> {
    let dir = destination.parent().unwrap();
    for parent in [dir.parent().unwrap(), dir] {
        match metadata(parent, true) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            info => {
                info?;
            }
        }
    }
    let installed = match metadata(destination, false) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        info => info?,
    };
    let mut source = File::open(source)?;
    if installed.len() != source.metadata()?.len() {
        return Ok(false);
    }
    let mut destination = File::open(destination)?;
    let mut original = [0; 8192];
    let mut copy = [0; 8192];
    let mut remaining = installed.len();
    while remaining != 0 {
        let chunk = remaining.min(original.len() as u64) as usize;
        source.read_exact(&mut original[..chunk])?;
        destination.read_exact(&mut copy[..chunk])?;
        if original[..chunk] != copy[..chunk] {
            return Ok(false);
        }
        remaining -= chunk as u64;
    }
    Ok(true)
}

pub(super) fn publish(source: &Path, destination: &Path) -> io::Result<()> {
    let dir = destination.parent().unwrap();
    directory(dir.parent().unwrap())?;
    directory(dir)?;
    let staging = dir.join(format!("{}.pending", std::process::id()));
    let mut incoming = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    let copied = io::copy(&mut incoming, &mut output).and_then(|_| output.sync_all());
    drop(output);
    let result = copied.and_then(|()| super::files::replace_staged_exe(&staging, destination));
    if result.is_err() {
        let _ = fs::remove_file(staging);
    }
    result
}
