use std::{ffi::OsStr, io, mem::size_of, os::windows::ffi::OsStrExt, path::Path, ptr::null};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ELEVATION_REQUIRED},
    System::Threading::{
        CREATE_NEW_CONSOLE, CreateProcessW, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
    },
};

use super::{
    Apartment,
    explorer::StandardDesktop,
    launch_channel::Channel,
    launch_packaged, packages, privilege,
    shortcut::{self, LaunchTarget},
};
use crate::{
    catalog::{Source, identity::executable_path},
    i18n::{self, Language, Msg, render, tr},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    operation: Operation,
    source: Source,
    language: Language,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum Operation {
    Launch,
    Reveal,
}

pub(super) fn launch(source: &Source) -> Result<(), String> {
    standard_action(source, Operation::Launch)
}

pub(super) fn reveal(source: &Source) -> Result<(), String> {
    standard_action(source, Operation::Reveal)
}

fn standard_action(source: &Source, operation: Operation) -> Result<(), String> {
    let current = privilege::current()
        .map_err(|error| render(Msg::LaunchCheckPrivilege, &[("detail", &error.to_string())]))?;
    if current.standard() {
        return execute(source, operation);
    }
    if !current.can_delegate() {
        return Err(tr(Msg::LaunchUnsupported).into());
    }
    let desktop = StandardDesktop::find(current.session).map_err(unavailable)?;
    let (mut channel, id) = Channel::server(&desktop.context).map_err(unavailable)?;
    let executable = std::env::current_exe().map_err(unavailable)?;
    desktop
        .start_worker(&executable, &id)
        .map_err(|error| standard_error(&error.to_string()))?;
    channel.connect().map_err(unavailable)?;
    let worker = channel.peer(true).map_err(unavailable)?;
    if worker.context().map_err(unavailable)? != desktop.context {
        return Err(standard_error(
            "the launch worker does not have the verified standard token",
        ));
    }
    worker.allow_foreground();
    channel
        .send(&Request {
            operation,
            source: source.clone(),
            language: i18n::current(),
        })
        .map_err(unavailable)?;
    if matches!(operation, Operation::Launch) {
        // The verified worker can be waiting for the user's UAC decision.
        channel.wait_for_consent();
    }
    channel.receive().map_err(unavailable)?
}

pub fn worker(id: &str) -> Result<(), String> {
    let current = privilege::current().map_err(unavailable)?;
    if !current.standard() {
        return Err(standard_error(
            "the launch worker is not running with a standard token",
        ));
    }
    let mut channel = Channel::client(id).map_err(unavailable)?;
    let caller = channel.peer(false).map_err(unavailable)?;
    if caller.session().map_err(unavailable)? != current.session {
        return Err(standard_error(
            "the launch worker and caller are in different sessions",
        ));
    }
    let request: Request = channel.receive().map_err(unavailable)?;
    i18n::set_language(Some(request.language));
    let response = (|| {
        let _apartment = Apartment::enter().map_err(|error| standard_error(&error.to_string()))?;
        execute(&request.source, request.operation)
    })();
    channel.restart_deadline();
    channel.send(&response).map_err(unavailable)
}

fn execute(source: &Source, operation: Operation) -> Result<(), String> {
    if matches!(operation, Operation::Reveal) {
        let current = privilege::current().map_err(unavailable)?;
        let desktop = StandardDesktop::find(current.session).map_err(unavailable)?;
        return super::reveal::reveal_standard(source, &desktop).map_err(|error| error.to_string());
    }
    match source {
        Source::Desktop(path) if executable_path(path).is_some() => {
            launch_executable(path, OsStr::new(""), None, None, || {
                super::elevate_desktop(path)
            })
        }
        Source::Desktop(path)
            if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("url")) =>
        {
            let (executable, arguments) = super::internet_shortcut::direct_target(path)
                .ok_or_else(|| tr(Msg::LaunchUnsupported).to_owned())?;
            create_process(&executable, &arguments, None, None).map_err(|error| {
                if error.raw_os_error() == Some(ERROR_ELEVATION_REQUIRED as i32) {
                    tr(Msg::LaunchElevationRequired).into()
                } else {
                    error.to_string()
                }
            })
        }
        Source::Desktop(path) => match shortcut::for_standard_launch(path) {
            Some(LaunchTarget::Desktop {
                path: target,
                arguments,
                directory,
                show,
            }) => launch_executable(&target, &arguments, directory.as_deref(), show, || {
                super::elevate_desktop(path)
            }),
            Some(LaunchTarget::Package(id)) => activate_package(&id),
            Some(LaunchTarget::RequiresElevation) => {
                super::elevate_desktop(path).map_err(|error| error.to_string())
            }
            None => Err(tr(Msg::LaunchUnsupported).into()),
        },
        Source::Packaged(id) => activate_package(id),
        Source::Steam { app_id, .. } => {
            let (client, arguments) = super::steam::launch_target(*app_id)
                .ok_or_else(|| tr(Msg::LaunchUnsupported).to_owned())?;
            create_process(&client, &arguments, None, None).map_err(|error| error.to_string())
        }
    }
}

fn activate_package(id: &str) -> Result<(), String> {
    if !packages::can_activate_standard(id) {
        return Err(tr(Msg::LaunchUnsupported).into());
    }
    launch_packaged(id).map_err(|error| error.to_string())
}

fn launch_executable(
    target: &Path,
    arguments: &OsStr,
    directory: Option<&Path>,
    show: Option<u16>,
    request_elevation: impl FnOnce() -> io::Result<()>,
) -> Result<(), String> {
    create_process(target, arguments, directory, show)
        .or_else(|error| {
            if error.raw_os_error() == Some(ERROR_ELEVATION_REQUIRED as i32) {
                // The selected shortcut retains its arguments and working
                // directory when Windows presents the application's UAC prompt.
                request_elevation()
            } else {
                Err(error)
            }
        })
        .map_err(|error| error.to_string())
}

fn create_process(
    path: &Path,
    arguments: &OsStr,
    directory: Option<&Path>,
    show: Option<u16>,
) -> io::Result<()> {
    let executable: Vec<_> = path.as_os_str().encode_wide().collect();
    if executable
        .iter()
        .any(|&unit| unit == 0 || unit == u16::from(b'"'))
    {
        return Err(io::Error::other(tr(Msg::LaunchUnsupported)));
    }
    let mut command = Vec::with_capacity(executable.len() + 32);
    command.push(u16::from(b'"'));
    command.extend_from_slice(&executable);
    command.push(u16::from(b'"'));
    if !arguments.is_empty() {
        command.push(u16::from(b' '));
        command.extend(arguments.encode_wide());
    }
    if command.len() >= 32767 || command.contains(&0) {
        return Err(io::Error::other(tr(Msg::LaunchUnsupported)));
    }
    command.push(0);
    let mut executable = executable;
    executable.push(0);
    let directory = directory.or_else(|| path.parent()).map(|path| {
        path.as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>()
    });
    let mut startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    if let Some(show) = show {
        startup.dwFlags = STARTF_USESHOWWINDOW;
        startup.wShowWindow = show;
    }
    let mut process = PROCESS_INFORMATION::default();
    // Console targets get their own usable console. No STARTF_USESTDHANDLES
    // or inheritable private pipe ends are passed to the target process.
    let started = unsafe {
        CreateProcessW(
            executable.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NEW_CONSOLE,
            null(),
            directory.as_ref().map_or(null(), |path| path.as_ptr()),
            &startup,
            &mut process,
        )
    };
    if started == 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

fn unavailable(error: io::Error) -> String {
    standard_error(&error.to_string())
}

fn standard_error(detail: &str) -> String {
    render(Msg::LaunchStandardUnavailable, &[("detail", detail)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_program_does_not_request_elevation() {
        let missing = std::env::temp_dir()
            .join("Rustart.Missing.Probe")
            .join("missing.exe");
        assert!(
            launch_executable(&missing, OsStr::new(""), None, None, || {
                panic!("an ordinary launch error requested elevation")
            })
            .is_err()
        );
    }

    #[test]
    #[ignore = "requires a standard token and RUSTART_TEST_ADMIN_EXE with a requireAdministrator manifest"]
    fn manifest_requiring_elevation_routes_to_the_consent_prompt() {
        let executable =
            std::path::PathBuf::from(std::env::var_os("RUSTART_TEST_ADMIN_EXE").unwrap());
        assert!(privilege::current().unwrap().standard());
        let mut requested = false;
        launch_executable(&executable, OsStr::new(""), None, None, || {
            requested = true;
            Ok(())
        })
        .unwrap();
        assert!(requested);
    }
}
