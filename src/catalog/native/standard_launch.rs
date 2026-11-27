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
    let (channel, id) = Channel::server(&desktop.context).map_err(unavailable)?;
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
    if matches!(operation, Operation::Reveal) {
        worker.allow_foreground();
    }
    channel
        .send(&Request {
            operation,
            source: source.clone(),
            language: i18n::current(),
        })
        .map_err(unavailable)?;
    channel.receive().map_err(unavailable)?
}

pub fn worker(id: &str) -> Result<(), String> {
    let current = privilege::current().map_err(unavailable)?;
    if !current.standard() {
        return Err(standard_error(
            "the launch worker is not running with a standard token",
        ));
    }
    let channel = Channel::client(id).map_err(unavailable)?;
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
            create_process(path, OsStr::new(""), None, None)
        }
        Source::Desktop(path)
            if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("url")) =>
        {
            let (executable, arguments) = super::internet_shortcut::direct_target(path)
                .ok_or_else(|| tr(Msg::LaunchUnsupported).to_owned())?;
            create_process(&executable, &arguments, None, None)
        }
        Source::Desktop(path) => match shortcut::for_standard_launch(path) {
            Some(LaunchTarget::Desktop {
                path,
                arguments,
                directory,
                show,
            }) => create_process(&path, &arguments, directory.as_deref(), show),
            Some(LaunchTarget::Package(id)) => activate_package(&id),
            Some(LaunchTarget::RequiresElevation) => Err(tr(Msg::LaunchElevationRequired).into()),
            None => Err(tr(Msg::LaunchUnsupported).into()),
        },
        Source::Packaged(id) => activate_package(id),
    }
}

fn activate_package(id: &str) -> Result<(), String> {
    if !packages::can_activate_standard(id) {
        return Err(tr(Msg::LaunchUnsupported).into());
    }
    launch_packaged(id).map_err(|error| error.to_string())
}

fn create_process(
    path: &Path,
    arguments: &OsStr,
    directory: Option<&Path>,
    show: Option<u16>,
) -> Result<(), String> {
    let executable: Vec<_> = path.as_os_str().encode_wide().collect();
    if executable
        .iter()
        .any(|&unit| unit == 0 || unit == u16::from(b'"'))
    {
        return Err(tr(Msg::LaunchUnsupported).into());
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
        return Err(tr(Msg::LaunchUnsupported).into());
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
        let error = io::Error::last_os_error();
        return Err(
            if error.raw_os_error() == Some(ERROR_ELEVATION_REQUIRED as i32) {
                tr(Msg::LaunchElevationRequired).into()
            } else {
                error.to_string()
            },
        );
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
