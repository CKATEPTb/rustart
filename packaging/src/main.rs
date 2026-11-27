//! Downloadable Windows release. Only this process opens the LZX cabinet.
//! The extracted Rustart handles installation, CLI commands and elevation as usual.

#[path = "../../src/install_path.rs"]
mod install_path;
#[path = "../../src/launch_context.rs"]
mod launch_context;
mod stage;

use std::{
    env,
    fs::{File, OpenOptions},
    io::{self, Cursor},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::Path,
    process::{Child, Command},
    ptr::null_mut,
};

use cab::Cabinet;
use sha2::{Digest, Sha256};
use windows_sys::Win32::{
    Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    },
    System::{
        LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories},
        Threading::CREATE_NO_WINDOW,
    },
    UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
};
use windows_sys::core::w;

const CABINET: &[u8] = include_bytes!(env!("RUSTART_RELEASE_CAB"));
const DIGEST: &[u8; 32] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.sha256"));
const SIZE: u64 = u64::from_le_bytes(*include_bytes!(concat!(env!("OUT_DIR"), "/payload.size")));

fn verified_image(path: &Path) -> io::Result<File> {
    let mut image = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = image.metadata()?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || metadata.len() != SIZE
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unexpected Rustart payload file",
        ));
    }
    let mut sha = Sha256::new();
    io::copy(&mut image, &mut sha)?;
    if &<[u8; 32]>::from(sha.finalize()) != DIGEST {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Rustart payload checksum mismatch",
        ));
    }
    Ok(image)
}

fn spawn(path: &Path, from_terminal: bool, origin: &Path) -> io::Result<Child> {
    let mut child = Command::new(path);
    child
        .args(env::args_os().skip(1))
        .env(launch_context::PACKAGE_ORIGIN, origin);
    if !from_terminal {
        child.creation_flags(CREATE_NO_WINDOW);
    }
    child.spawn()
}

fn spawn_installed(from_terminal: bool, origin: &Path) -> io::Result<Child> {
    let directory = stage::program_files()?.join(r"CKATEPTb\Rustart");
    let _parents = install_path::pin_ancestors(&directory)?;
    let path = directory.join("rustart.exe");
    let _image = verified_image(&path)?;
    spawn(&path, from_terminal, origin)
}

fn run(from_terminal: bool) -> io::Result<i32> {
    if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let origin = env::current_exe()?;
    // Public commands can reuse this exact release. Private worker and
    // installer switches continue through the same staging path as before.
    let public_command = env::args_os().nth(1).is_none_or(|arg| {
        !arg.as_encoded_bytes().starts_with(b"--") || arg == "--help" || arg == "--version"
    });
    if public_command
        && matches!(stage::elevated(), Ok(false))
        && let Ok(mut child) = spawn_installed(from_terminal, &origin)
    {
        return Ok(child.wait()?.code().unwrap_or(1));
    }
    let stage = stage::Stage::new()?;
    {
        let mut cabinet = Cabinet::new(Cursor::new(CABINET))?;
        let mut file = stage.create_file()?;
        io::copy(&mut cabinet.read_file("rustart.exe")?, &mut file)?;
    }
    // Hold the verified image against writes and renames until the child and
    // its elevated installation have finished using the staged path.
    let _image = verified_image(stage.path())?;
    let status = spawn(stage.path(), from_terminal, &origin)?.wait()?;
    Ok(status.code().unwrap_or(1))
}

fn main() {
    let from_terminal = launch_context::launched_from_terminal();
    match run(from_terminal) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            let message = format!("Could not open the Rustart release: {error}");
            if from_terminal {
                eprintln!("{message}");
            } else {
                let message: Vec<_> = message.encode_utf16().chain([0]).collect();
                unsafe {
                    MessageBoxW(
                        null_mut(),
                        message.as_ptr(),
                        w!("Rustart"),
                        MB_OK | MB_ICONERROR,
                    )
                };
            }
            std::process::exit(1);
        }
    }
}
