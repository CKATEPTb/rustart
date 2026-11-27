use crate::{
    catalog::current_user_sid,
    i18n::{Msg, render, tr},
    install::{consent, files},
};
use std::process::ExitCode;

use super::{failure, scheduler};

const ENABLE: &str = "--install-admin-startup";
const DISABLE: &str = "--remove-admin-startup";
const ACCOUNT_MISMATCH: u8 = 2;

pub(super) fn sid() -> Result<String, String> {
    current_user_sid().map_err(failure)
}

fn apply(enable: bool, sid: &str) -> Result<(), String> {
    if !files::is_installed_image().map_err(failure)? {
        return Err(tr(Msg::StartupRequiresInstalled).into());
    }
    if enable {
        scheduler::install(sid)
    } else {
        scheduler::remove(sid)
    }
}

// The CLI can select only a fixed operation for the caller's own account.
// Task Scheduler always derives the executable and task name internally.
pub(super) fn handle_helper(args: &[String]) -> Option<ExitCode> {
    let enable = match args.first()?.as_str() {
        ENABLE => true,
        DISABLE => false,
        _ => return None,
    };
    let [_, requested_sid] = args else {
        return Some(ExitCode::FAILURE);
    };
    if cfg!(feature = "installation-fixture") {
        return Some(ExitCode::FAILURE);
    }
    if !matches!(consent::elevated(), Ok(true)) || sid().as_ref().ok() != Some(requested_sid) {
        return Some(ExitCode::from(ACCOUNT_MISMATCH));
    }
    Some(match apply(enable, requested_sid) {
        Ok(()) => ExitCode::SUCCESS,
        Err(detail) => {
            eprintln!("{detail}");
            ExitCode::FAILURE
        }
    })
}

pub(super) fn configure(enable: bool, sid: &str) -> Result<bool, String> {
    if cfg!(feature = "installation-fixture") || !files::is_installed_image().map_err(failure)? {
        return Err(tr(Msg::StartupRequiresInstalled).into());
    }
    if consent::elevated().map_err(failure)? {
        apply(enable, sid)?;
        return Ok(true);
    }
    let flag = if enable { ENABLE } else { DISABLE };
    match consent::request(&format!("{flag} {sid}")).map_err(failure)? {
        Some(0) => Ok(true),
        None => Ok(false),
        Some(status) if status == u32::from(ACCOUNT_MISMATCH) => {
            Err(tr(Msg::StartupSameAccount).into())
        }
        Some(status) => Err(render(
            Msg::StartupTaskRequestFailed,
            &[("status", &status.to_string())],
        )),
    }
}
