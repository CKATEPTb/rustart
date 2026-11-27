mod calc;
#[cfg(windows)]
mod i18n;
#[cfg(windows)]
mod prefs;

#[cfg(windows)]
fn run(args: &[String]) -> Result<(), String> {
    use i18n::{Msg, render, tr};

    if let Ok(prefs) = prefs::load() {
        i18n::set_language(prefs.language);
    }
    match args.split_first() {
        Some((verb, [])) if matches!(verb.as_str(), "-V" | "--version") => {
            println!("rustart {}", env!("CARGO_PKG_VERSION"));
        }
        Some((verb, rest)) if verb == "calc" => {
            let input = rest.join(" ");
            if input.trim().is_empty() {
                return Err(tr(Msg::CliMissingInput).into());
            }
            let result = calc::evaluate(&input).ok_or_else(|| tr(Msg::CliExpectedExpression))??;
            println!("{result}");
        }
        Some((verb, _)) => return Err(render(Msg::CliUnknownCommand, &[("command", verb)])),
        None => return Err(tr(Msg::CliMissingInput).into()),
    }
    Ok(())
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Rustart: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Rustart requires Windows.");
    std::process::exit(1);
}
