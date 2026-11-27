//! One executable provides the short-lived CLI client and the resident Win32 launcher.

mod calc;
#[cfg(windows)]
mod catalog;
#[cfg(windows)]
mod i18n;
#[cfg(windows)]
mod install;
#[cfg(windows)]
mod install_path;
#[cfg(windows)]
mod launch_context;
#[cfg(windows)]
mod overlay;
#[cfg(windows)]
mod prefs;
#[cfg(windows)]
mod runtime;
#[cfg(windows)]
mod startup;
#[cfg(windows)]
mod tray;

#[cfg(windows)]
fn diagnostic(message: impl std::fmt::Display) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "Rustart: {message}");
}

#[cfg(windows)]
fn run(args: &[String]) -> Result<bool, String> {
    use i18n::{Msg, render, tr};

    let verb = args.first().map(String::as_str);
    let rest = args.get(1..).unwrap_or_default();
    match (verb, rest) {
        (Some("--launch-normal-worker"), [channel]) => {
            return catalog::standard_launch_worker(channel).map(|()| true);
        }
        (Some("--catalog-worker"), []) => return catalog::write_discovery().map(|_| true),
        (Some("--shell-action-worker"), []) => {
            return overlay::run_shell_action_worker().map(|_| true);
        }
        (Some("--icon-worker"), []) => return overlay::run_icon_worker().map(|_| true),
        (Some("--resident"), []) => return runtime::serve().map(|_| true),
        (Some("--version" | "-V"), []) => {
            println!("rustart {}", env!("CARGO_PKG_VERSION"));
            return Ok(true);
        }
        _ => {}
    }
    let prefs = prefs::load();
    if let Ok(prefs) = &prefs {
        i18n::set_language(prefs.language);
    }

    if matches!(verb, Some("calc" | "search")) {
        let input = rest.join(" ");
        if input.trim().is_empty() {
            return Err(tr(Msg::CliMissingInput).into());
        }
        if verb == Some("calc") {
            let result = calc::evaluate(&input).ok_or_else(|| tr(Msg::CliExpectedExpression))??;
            println!("{result}");
        } else {
            prefs?;
            let apps = catalog::discover()?;
            for i in catalog::search(&apps, &input) {
                let app = &apps[i];
                let detail = match (app.detail.as_str(), &app.source) {
                    ("Меню «Пуск»", catalog::Source::Desktop(_)) => tr(Msg::CliStartMenu),
                    ("Приложение Windows", catalog::Source::Packaged(_)) => {
                        tr(Msg::CliWindowsApp)
                    }
                    (detail, _) => detail,
                };
                println!("{}\t{detail}", app.name);
            }
        }
        return Ok(true);
    }
    if !rest.is_empty() {
        return Err(tr(Msg::CliExtraArguments).into());
    }
    match verb {
        None | Some("show") => runtime::client(runtime::SHOW, true),
        Some("background") => runtime::client(runtime::PING, true),
        Some(startup::USER_LOGON) => {
            if startup::run_at_logon()? {
                runtime::client(runtime::PING, true)
            } else {
                Ok(true)
            }
        }
        Some(startup::ADMIN_LOGON) => {
            if startup::scheduled_at_logon()? {
                runtime::elevated_background()
            } else {
                Ok(true)
            }
        }
        Some("background-admin") => runtime::elevated_startup(),
        Some("--elevated-background") => runtime::elevated_background(),
        Some("hide") => runtime::client(runtime::HIDE, false),
        Some("refresh") => runtime::client(runtime::REFRESH, false),
        Some("quit") => runtime::client(runtime::QUIT, false),
        Some("status") => {
            if let Some(pid) = runtime::status()? {
                println!("running {pid}");
                Ok(true)
            } else {
                println!("stopped");
                Ok(false)
            }
        }
        Some("help" | "--help" | "-h") => {
            println!(
                "Rustart {}\n\n{}",
                env!("CARGO_PKG_VERSION"),
                tr(Msg::CliHelp)
            );
            Ok(true)
        }
        Some(command) => Err(render(Msg::CliUnknownCommand, &[("command", command)])),
    }
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use windows_sys::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
    };
    unsafe {
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Some(status) = startup::handle_helper(&args) {
        return status;
    }
    let graphical = !launch_context::launched_from_terminal();
    let result = install::handle_helper(&args).unwrap_or_else(|| {
        match install::dispatch(&args, graphical)? {
            Some(done) => Ok(done),
            None => run(&args),
        }
    });
    match result {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(message) => {
            diagnostic(&message);
            if (graphical && args.is_empty())
                || args.first().is_some_and(|a| a == "background-admin")
            {
                runtime::error_box(std::ptr::null_mut(), &message);
            }
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Rustart requires Windows.");
    std::process::exit(1);
}
