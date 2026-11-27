use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf, Prefix},
    ptr::{null, null_mut},
    slice,
};

use windows::{
    Win32::{
        Foundation::S_OK,
        System::Com::{
            CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ, STGM_SHARE_DENY_NONE,
        },
        UI::Shell::{
            AL_EFFECTIVE, ASSOCF_INIT_FIXED_PROGID, ASSOCF_NOFIXUPS, ASSOCSTR, ASSOCSTR_COMMAND,
            ASSOCSTR_EXECUTABLE, AT_URLPROTOCOL, ApplicationAssociationRegistration,
            AssocQueryStringW, CLSID_InternetShortcut, CommandLineToArgvW,
            IApplicationAssociationRegistration, IUniformResourceLocatorW,
        },
    },
    core::{Interface, PCWSTR, PWSTR, w},
};
use windows_sys::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, LocalFree},
        System::Registry::{
            HKEY, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegGetValueW, RegOpenKeyExW,
            RegQueryValueExW,
        },
        UI::Shell::{ASSOCKEY_CLASS, AssocQueryKeyW},
    },
    core::w as ws,
};

use super::{owned_string, privilege, shortcut};
use crate::catalog::identity::executable_path;

const FLAGS: windows::Win32::UI::Shell::ASSOCF =
    windows::Win32::UI::Shell::ASSOCF(ASSOCF_INIT_FIXED_PROGID.0 | ASSOCF_NOFIXUPS.0);

pub(super) fn direct_target(source: &Path) -> Option<(PathBuf, OsString)> {
    if !source.is_absolute() || !source.extension()?.eq_ignore_ascii_case("url") {
        return None;
    }
    let uri = read_shortcut(source)?;
    resolve_association(&uri)
}

fn read_shortcut(source: &Path) -> Option<String> {
    let path: Vec<_> = source.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let link: IUniformResourceLocatorW =
            CoCreateInstance(&CLSID_InternetShortcut, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        file.Load(PCWSTR(path.as_ptr()), STGM_READ | STGM_SHARE_DENY_NONE)
            .ok()?;
        let uri = link.GetURL().ok()?;
        if uri.is_null() {
            return None;
        }
        owned_string(uri).into_string().ok()
    }
}

fn resolve_association(uri: &str) -> Option<(PathBuf, OsString)> {
    let protocol = protocol(uri)?;
    let query: Vec<_> = protocol.encode_utf16().chain([0]).collect();
    let class: Vec<_> = unsafe {
        let registry: IApplicationAssociationRegistration = CoCreateInstance(
            &ApplicationAssociationRegistration,
            None,
            CLSCTX_INPROC_SERVER,
        )
        .ok()?;
        let id = registry
            .QueryCurrentDefault(PCWSTR(query.as_ptr()), AT_URLPROTOCOL, AL_EFFECTIVE)
            .ok()?;
        if id.is_null() {
            return None;
        }
        owned_string(id).encode_wide().chain([0]).collect()
    };
    if !plain_open_verb(&class)? {
        return None;
    }
    let command = assoc_text(&class, ASSOCSTR_COMMAND)?;
    let executable = PathBuf::from(assoc_text(&class, ASSOCSTR_EXECUTABLE)?);
    let launch = direct_command(&command, &executable, uri)?;
    // Do not accept a symbolic link to an interpreter or a handler redirected
    // outside local storage. This only reads metadata, without opening the URL.
    let verified_file = shortcut::resolve_local_file(&launch.0)?;
    if shell_proxy(&verified_file)? {
        return None;
    }
    Some(launch)
}

fn protocol(uri: &str) -> Option<&'static str> {
    // The raw substitution must be one argument for both ordinary Windows
    // parsers and Chromium's literal --single-argument suffix.
    if uri
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\\'))
    {
        return None;
    }
    let (scheme, remainder) = uri.split_once("://")?;
    if remainder.is_empty() {
        return None;
    }
    if scheme.eq_ignore_ascii_case("http") {
        return Some("http");
    }
    if scheme.eq_ignore_ascii_case("https") {
        return Some("https");
    }
    if scheme.eq_ignore_ascii_case("steam")
        && let Some((action, app)) = remainder.split_once('/')
        && matches!(action, "rungameid" | "run")
        && !app.is_empty()
        && app.bytes().all(|c| c.is_ascii_digit())
    {
        return Some("steam");
    }
    None
}

fn assoc_text(class: &[u16], field: ASSOCSTR) -> Option<OsString> {
    let mut output = vec![0; 32768];
    let mut length = output.len() as u32;
    let result = unsafe {
        AssocQueryStringW(
            FLAGS,
            field,
            PCWSTR(class.as_ptr()),
            w!("open"),
            Some(PWSTR(output.as_mut_ptr())),
            &mut length,
        )
    };
    (result == S_OK).then(|| terminated(&output)).flatten()
}

fn plain_open_verb(class: &[u16]) -> Option<bool> {
    let mut key = null_mut();
    if unsafe { AssocQueryKeyW(FLAGS.0, ASSOCKEY_CLASS, class.as_ptr(), null(), &mut key) }
        != S_OK.0
    {
        return None;
    }
    let class = Key(key);
    let shell = class.open(ws!("shell"))?;
    let verb = shell.open(ws!("open"))?;
    let command = verb.open(ws!("command"))?;
    let mut value = [0; 64];
    let mut bytes = size_of_val(&value) as u32;
    let status = unsafe {
        RegGetValueW(
            shell.0,
            null(),
            null(),
            RRF_RT_REG_SZ,
            null_mut(),
            value.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    let default = match status {
        ERROR_FILE_NOT_FOUND => OsString::new(),
        ERROR_SUCCESS => terminated(&value)?,
        _ => return None,
    };
    let direct = verb.absent_value(ws!("ExplorerCommandHandler"))
        && verb.absent_value(ws!("DelegateExecute"))
        && verb.absent_key(ws!("ddeexec"))
        && verb.absent_key(ws!("DropTarget"))
        && command.absent_value(ws!("DelegateExecute"))
        && command.absent_value(ws!("ExplorerCommandHandler"));
    Some(use_plain_open(&default, direct))
}

fn use_plain_open(default: &std::ffi::OsStr, direct: bool) -> bool {
    direct && (default.is_empty() || default.eq_ignore_ascii_case("open"))
}

struct Key(HKEY);

impl Key {
    fn open(&self, subkey: *const u16) -> Option<Self> {
        let mut result = null_mut();
        (unsafe { RegOpenKeyExW(self.0, subkey, 0, KEY_READ, &mut result) } == ERROR_SUCCESS)
            .then_some(Self(result))
    }

    fn absent_key(&self, subkey: *const u16) -> bool {
        let mut key = null_mut();
        let code = unsafe { RegOpenKeyExW(self.0, subkey, 0, KEY_READ, &mut key) };
        if code == ERROR_SUCCESS {
            unsafe { RegCloseKey(key) };
        }
        matches!(code, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
    }

    fn absent_value(&self, name: *const u16) -> bool {
        let mut bytes = 0;
        (unsafe { RegQueryValueExW(self.0, name, null(), null_mut(), null_mut(), &mut bytes) })
            == ERROR_FILE_NOT_FOUND
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { RegCloseKey(self.0) };
        }
    }
}

fn direct_command(
    command: &std::ffi::OsStr,
    executable: &Path,
    uri: &str,
) -> Option<(PathBuf, OsString)> {
    protocol(uri)?;
    let args = split_command(command)?;
    let (application, tail) = args.split_first()?;
    let application = Path::new(application);
    executable_path(application)?;
    if !matches!(application.components().next(), Some(Component::Prefix(prefix))
        if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        || !privilege::paths_equal(application, executable)
        || shell_proxy(application)?
    {
        return None;
    }
    let (last, fixed) = tail.split_last()?;
    if !last.eq_ignore_ascii_case("%1") && !last.eq_ignore_ascii_case("%l") {
        return None;
    }
    let prefix = match fixed {
        [] => "",
        [argument] if argument == "--single-argument" => "--single-argument ",
        [first, second] if first == "-osint" && second == "-url" => "-osint -url ",
        _ => return None,
    };
    Some((application.into(), format!("{prefix}{uri}").into()))
}

fn shell_proxy(path: &Path) -> Option<bool> {
    let name = path.file_name()?;
    Some(
        [
            "cmd.exe",
            "powershell.exe",
            "pwsh.exe",
            "wscript.exe",
            "cscript.exe",
            "mshta.exe",
            "rundll32.exe",
            "regsvr32.exe",
            "explorer.exe",
            "openwith.exe",
            "control.exe",
            "msiexec.exe",
        ]
        .iter()
        .any(|proxy| name.eq_ignore_ascii_case(proxy)),
    )
}

fn split_command(command: &std::ffi::OsStr) -> Option<Vec<OsString>> {
    if command.is_empty() {
        return None;
    }
    let wide: Vec<_> = command.encode_wide().chain([0]).collect();
    let mut length = 0;
    let result = unsafe { CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut length) };
    if result.is_null() {
        return None;
    }
    let args = unsafe {
        let strings = slice::from_raw_parts(result, length as usize);
        let args = strings
            .iter()
            .map(|arg| OsString::from_wide(arg.as_wide()))
            .collect();
        LocalFree(result.cast());
        args
    };
    Some(args)
}

fn terminated(input: &[u16]) -> Option<OsString> {
    input
        .iter()
        .position(|&unit| unit == 0)
        .map(|end| OsString::from_wide(&input[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, fs};

    #[test]
    fn uses_the_registered_executable_and_keeps_the_url_in_one_argument() {
        let exe = Path::new(r"C:\Program Files\Browser\browser.exe");
        let uri = "https://example.invalid/search?a=1&b=two%20words";
        let command = OsStr::new(r#""C:\Program Files\Browser\browser.exe" --single-argument %1"#);
        let (handler, arguments) = direct_command(command, exe, uri).unwrap();
        assert_eq!(handler, exe);
        assert_eq!(
            arguments,
            OsString::from(format!("--single-argument {uri}"))
        );

        let steam = Path::new(r"C:\Program Files\Steam\steam.exe");
        let steam_command = OsStr::new(r#""C:\Program Files\Steam\steam.exe" "%1""#);
        let (_, game) = direct_command(steam_command, steam, "steam://rungameid/123").unwrap();
        assert_eq!(game, "steam://rungameid/123");
        assert!(direct_command(command, steam, uri).is_none());
    }

    #[test]
    fn rejects_command_lines_and_uris_outside_the_supported_direct_format() {
        let browser = Path::new(r"C:\Apps\browser.exe");
        let normal = OsStr::new(r#"C:\Apps\browser.exe "%1""#);
        for uri in [
            "https://example.invalid/a b",
            "https://example.invalid/\"--flag",
            r"https://example.invalid/a\b",
            "https://example.invalid/\n--flag",
            "shell://AppsFolder",
            "steam://open/console",
            "steam://run/123//extra",
        ] {
            assert!(direct_command(normal, browser, uri).is_none(), "{uri:?}");
        }
        for command in [
            r#"C:\Apps\browser.exe --extra "%1""#,
            r#"C:\Apps\browser.exe %1 %1"#,
            r#"C:\Apps\browser.exe --url=%1"#,
            r#"C:\Apps\browser.exe %*"#,
            r#"C:\Apps\browser.exe %1 --single-argument"#,
        ] {
            assert!(
                direct_command(OsStr::new(command), browser, "https://example.invalid/").is_none(),
                "{command}"
            );
        }
        for name in [
            "cmd.exe",
            "PowerShell.exe",
            "wscript.exe",
            "mshta.exe",
            "explorer.exe",
        ] {
            let path = PathBuf::from(r"C:\Windows").join(name);
            let command = format!(r#""{}" "%1""#, path.display());
            assert!(direct_command(command.as_ref(), &path, "https://example.invalid/").is_none());
        }
    }

    #[test]
    fn a_different_default_verb_or_delegated_open_is_not_used() {
        assert!(use_plain_open(OsStr::new(""), true));
        assert!(use_plain_open(OsStr::new("OPEN"), true));
        assert!(!use_plain_open(OsStr::new("runas"), true));
        assert!(!use_plain_open(OsStr::new("custom"), true));
        assert!(!use_plain_open(OsStr::new("open"), false));
    }

    #[test]
    fn loads_a_saved_internet_shortcut_as_metadata() {
        let _apartment = super::super::Apartment::enter().unwrap();
        let name = format!("rustart-url-read-{}.url", std::process::id());
        let path = std::env::current_exe().unwrap().with_file_name(name);
        fs::write(
            &path,
            b"[InternetShortcut]\r\nURL=https://example.invalid/?a=1&b=2\r\n",
        )
        .unwrap();
        let uri = read_shortcut(&path);
        fs::remove_file(&path).unwrap();
        assert_eq!(uri.as_deref(), Some("https://example.invalid/?a=1&b=2"));
    }

    #[test]
    #[ignore = "requires a directly registered default HTTP(S) handler"]
    fn resolves_the_effective_default_browser_without_opening_it() {
        let _apartment = super::super::Apartment::enter().unwrap();
        for uri in [
            "http://example.invalid/?a=1&b=2",
            "https://example.invalid/?a=1&b=2",
        ] {
            let (handler, arguments) =
                resolve_association(uri).expect("a verified direct browser handler");
            assert!(arguments.to_string_lossy().ends_with(uri));
            eprintln!("Verified direct handler: {}", handler.display());
        }
    }
}
