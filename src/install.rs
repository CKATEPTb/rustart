pub(crate) mod consent;
pub(crate) mod files;
#[cfg(feature = "installation-fixture")]
mod fixture;

use crate::{
    i18n::{self, Msg, render, tr},
    launch_context::{self, PACKAGE_ORIGIN},
    runtime,
};
use std::{
    env, io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Path, PathBuf},
    process::Command,
    ptr::null,
};
use windows_sys::{
    Win32::{
        Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{
            CreateMutexExW, MUTEX_MODIFY_STATE, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE,
            WaitForSingleObject,
        },
    },
    core::w,
};

const INSTALL: &str = "--install-application";

struct InstallLock(OwnedHandle);

impl InstallLock {
    fn acquire() -> io::Result<Self> {
        let raw = unsafe {
            CreateMutexExW(
                null(),
                w!(r"Local\CKATEPTb.Rustart.Install.v1"),
                0,
                SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE,
            )
        };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        match unsafe { WaitForSingleObject(handle.as_raw_handle(), 120_000) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
            WAIT_TIMEOUT => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Timed out waiting for another Rustart installation.",
            )),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        unsafe { ReleaseMutex(self.0.as_raw_handle()) };
    }
}

// This process receives no paths, application commands or settings from the caller.
// Its only operation is to publish its own mapped image to the fixed destination.
pub(crate) fn handle_helper(args: &[String]) -> Option<Result<bool, String>> {
    #[cfg(feature = "installation-fixture")]
    if matches!(args, [flag] if flag == "--installation-fixture") {
        return Some((|| {
            if env::var_os(PACKAGE_ORIGIN).is_some() {
                let _source = pin_current_source()?;
            }
            println!("rustart-installation-fixture-v1");
            Ok(true)
        })());
    }
    if args.first().is_none_or(|arg| arg != INSTALL) {
        return None;
    }
    if args.len() != 1 || cfg!(feature = "installation-fixture") {
        return Some(Err("Unexpected Rustart installer invocation.".into()));
    }
    Some(install_elevated().map(|()| true))
}

fn io_error(error: io::Error) -> String {
    error.to_string()
}

fn install_elevated() -> Result<(), String> {
    if !consent::elevated().map_err(io_error)? {
        return Err(tr(Msg::InstallConsentRequired).into());
    }
    files::install_current_exe().map_err(io_error)
}

fn request_publication() -> Result<(), String> {
    if consent::elevated().map_err(io_error)? {
        return install_elevated();
    }
    let _source = pin_current_source()?;
    match consent::request(INSTALL).map_err(io_error)? {
        Some(0) => Ok(()),
        Some(status) => Err(render(
            Msg::InstallProcessFailed,
            &[("status", &status.to_string())],
        )),
        None => Err(tr(Msg::InstallCancelled).into()),
    }
}

fn pin_current_source() -> Result<(Vec<std::fs::File>, std::fs::File), String> {
    let source = env::current_exe().map_err(io_error)?;
    let parents = crate::install_path::pin_ancestors(
        source
            .parent()
            .ok_or_else(|| "The Rustart executable has no parent directory.".to_owned())?,
    )
    .map_err(io_error)?;
    let image = files::running_source().map_err(io_error)?;
    Ok((parents, image))
}

fn launch(destination: &Path, args: &[String], silent: bool) -> Result<bool, String> {
    let mut command = Command::new(destination);
    command.args(args).env_remove(PACKAGE_ORIGIN);
    if silent {
        launch_context::configure_graphical_child(&mut command);
    }
    command
        .status()
        .map(|status| status.success())
        .map_err(io_error)
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || a.canonicalize()
            .ok()
            .zip(b.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
}

fn handoff(
    #[cfg(feature = "installation-fixture")] source: &Path,
    destination: &Path,
    fixture: bool,
    args: &[String],
    silent: bool,
) -> Result<bool, String> {
    if let Ok(prefs) = crate::prefs::load() {
        i18n::set_language(prefs.language);
    }
    let _lock = InstallLock::acquire().map_err(io_error)?;
    let matches = if fixture {
        #[cfg(feature = "installation-fixture")]
        {
            self::fixture::matches(source, destination).map_err(io_error)?
        }
        #[cfg(not(feature = "installation-fixture"))]
        {
            unreachable!()
        }
    } else {
        files::matches_current_exe().map_err(io_error)?
    };

    let previous = runtime::installation_owner()?;
    if !matches {
        if fixture {
            #[cfg(feature = "installation-fixture")]
            self::fixture::publish(source, destination).map_err(io_error)?;
        } else {
            request_publication()?;
            if !files::matches_current_exe().map_err(io_error)? {
                return Err(tr(Msg::InstallMismatch).into());
            }
        }
    }

    let previous = match previous {
        // A launch while the consent dialog was open can elect an older,
        // portable resident. A resident from the installed path may already
        // be running the published image.
        None if !matches => runtime::installation_owner()?.filter(|owner| {
            owner
                .image
                .as_deref()
                .is_some_and(|p| !same_path(p, destination))
        }),
        owner => owner,
    };
    let previous = previous.filter(|owner| {
        // The fixture runs on a private destination and may only stop its own owner.
        !fixture
            || owner
                .image
                .as_deref()
                .and_then(|p| p.canonicalize().ok())
                .is_some_and(|p| p.starts_with(destination.ancestors().nth(4).unwrap()))
    });

    let mut stopped = false;
    if let Some(owner) = previous
        && (!matches
            || owner
                .image
                .as_deref()
                .is_some_and(|p| !same_path(p, destination)))
    {
        stopped = owner.quit()?;
    }

    let verb = args.first().map(String::as_str);
    if stopped && verb == Some("quit") {
        return Ok(true);
    }
    if stopped
        && !matches!(
            verb,
            None | Some("show" | "background" | "background-admin")
        )
        && !launch(destination, &["background".into()], silent)?
    {
        return Err("Could not restart the installed Rustart instance.".into());
    }
    launch(destination, args, silent)
}

pub(crate) fn dispatch(args: &[String], silent: bool) -> Result<Option<bool>, String> {
    let verb = args.first().map(String::as_str);
    let rest = args.get(1..).unwrap_or_default();
    match (verb, rest) {
        (Some("--catalog-worker" | "--shell-action-worker" | "--icon-worker"), [])
        | (Some("--launch-normal-worker"), [_]) => return Ok(None),
        (Some("calc" | "search"), input) if input.iter().any(|word| !word.trim().is_empty()) => {}
        (
            None
            | Some(
                "show"
                | "background"
                | "background-admin"
                | crate::startup::USER_LOGON
                | crate::startup::ADMIN_LOGON
                | "hide"
                | "refresh"
                | "quit"
                | "status"
                | "--version"
                | "-V"
                | "help"
                | "--help"
                | "-h"
                | "--resident"
                | "--elevated-background",
            ),
            [],
        ) => {}
        _ => return Ok(None),
    }
    let source = env::current_exe().map_err(io_error)?;
    let origin = env::var_os(PACKAGE_ORIGIN)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());

    #[cfg(feature = "installation-fixture")]
    let (destination, fixture) = {
        let Some(destination) =
            fixture::destination(&source, origin.as_deref()).map_err(io_error)?
        else {
            return Ok(None);
        };
        (destination, true)
    };
    #[cfg(not(feature = "installation-fixture"))]
    let (destination, fixture) = (files::installed_exe().map_err(io_error)?, false);

    let installed = same_path(&source, &destination)
        && (fixture || files::is_installed_image().map_err(io_error)?);
    match verb {
        // A stale logon entry must never start an installer or prompt at sign-in.
        Some(crate::startup::USER_LOGON | crate::startup::ADMIN_LOGON) => {
            Ok((!installed).then_some(true))
        }
        Some("--resident" | "--elevated-background") if !installed => {
            Err("Rustart workers require the installed executable.".into())
        }
        Some("--resident" | "--elevated-background") => Ok(None),
        _ if installed && origin.is_none() => Ok(None),
        _ => handoff(
            #[cfg(feature = "installation-fixture")]
            &source,
            &destination,
            fixture,
            args,
            silent,
        )
        .map(Some),
    }
}
