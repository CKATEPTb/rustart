use std::{
    fs::File,
    io::{self, Read, Write},
    os::windows::{io::FromRawHandle, process::CommandExt},
    process::{Child, Command, Stdio},
    ptr::null_mut,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use windows_sys::Win32::{
    Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation},
    System::{
        Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
        },
        Threading::CREATE_NO_WINDOW,
    },
    UI::WindowsAndMessaging::AllowSetForegroundWindow,
};

use crate::{
    catalog::{self, App, Source},
    i18n::{self, Msg, WORKER_LANGUAGE, render, tr},
};

const PROTOCOL: u8 = 1;
const MAX_REQUEST: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 64 * 1024;
const MAX_DIAGNOSTIC: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum ShellOperation {
    Launch(bool),
    Reveal,
}

impl ShellOperation {
    pub(super) fn failure(self) -> &'static str {
        match self {
            Self::Launch(_) => tr(Msg::ShellOpenError),
            Self::Reveal => tr(Msg::ShellFolderError),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ShellAction {
    pub(super) operation: ShellOperation,
    name: String,
    source: Source,
}

impl ShellAction {
    pub(super) fn new(app: &App, operation: ShellOperation) -> Self {
        Self {
            operation,
            name: app.name.clone(),
            source: app.source.clone(),
        }
    }

    pub(super) fn execute_isolated(self) -> Result<(), String> {
        let action_error = self.operation.failure();
        let domain_error = |error: io::Error| format!("{action_error} {error}");
        let executable = std::env::current_exe().map_err(domain_error)?;
        // Run the same binary directly under this process's token. The child
        // delegates non-elevated actions to Explorer when the token is elevated.
        // A filesystem path or application ID never appears on the command line.
        let mut child = Command::new(executable)
            .arg("--shell-action-worker")
            .env(WORKER_LANGUAGE, i18n::current().tag())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(domain_error)?;

        // Only the user's selected helper may bring the requested application
        // forward. Windows can refuse the grant if Rustart lost focus.
        unsafe {
            AllowSetForegroundWindow(child.id());
        }
        let response = exchange(&mut child, &self);
        // Reap only the helper. A shell action may begin a long-lived process
        // or display UAC, so this path has no job object or forced timeout.
        let status = child.wait().map_err(domain_error)?;
        if !status.success() {
            let detail = render(Msg::ShellHelperError, &[("status", &status.to_string())]);
            return Err(format!("{action_error} {detail}"));
        }
        response.map_err(domain_error)?
    }
}

fn exchange(child: &mut Child, request: &ShellAction) -> io::Result<Result<(), String>> {
    let response_pipe = child.stdout.take();
    let mut request_pipe = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("Канал запроса недоступен."))?;
    let response_pipe =
        response_pipe.ok_or_else(|| io::Error::other("Канал ответа недоступен."))?;
    // Both handles are owned here, so a write failure also closes stdin and
    // drops the response reader before the caller waits on the helper.
    write_packet(&mut request_pipe, request, MAX_REQUEST)?;
    drop(request_pipe);
    read_packet(response_pipe, MAX_RESPONSE)
}

pub(crate) fn run_worker() -> Result<(), String> {
    i18n::select_worker_language();
    let reply =
        take_reply_pipe().map_err(|error| format!("Канал ответа оболочки недоступен. {error}"))?;
    let request = read_packet::<ShellAction>(io::stdin().lock(), MAX_REQUEST);
    let response = match request {
        Ok(request) => match private_standard_handles() {
            Ok(()) => match request.operation {
                ShellOperation::Launch(elevated) => {
                    catalog::launch(&request.source, &request.name, elevated)
                }
                ShellOperation::Reveal => catalog::reveal(&request.source, &request.name),
            },
            Err(error) => Err(format!("{} {error}", request.operation.failure())),
        },
        Err(error) => Err(format!("Не удалось прочитать действие. {error}")),
    };
    let response = response.map_err(|mut error| {
        error.truncate(error.floor_char_boundary(MAX_DIAGNOSTIC));
        error
    });
    write_packet(reply, &response, MAX_RESPONSE)
        .map_err(|error| format!("Не удалось передать ответ оболочки. {error}"))
}

fn take_reply_pipe() -> io::Result<File> {
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::other("Нет канала вывода."));
    }
    // Keep the reply private even if a shell extension writes to the helper's
    // standard output or explicitly passes standard handles to a descendant.
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0
        || unsafe { SetStdHandle(STD_OUTPUT_HANDLE, null_mut()) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn private_standard_handles() -> io::Result<()> {
    // These handles connect the helper and its parent. An application started
    // by the helper must not get copies, including when the shell inherits stdio.
    for id in [STD_INPUT_HANDLE, STD_ERROR_HANDLE] {
        let handle = unsafe { GetStdHandle(id) };
        if !handle.is_null()
            && handle != INVALID_HANDLE_VALUE
            && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn write_packet(mut sink: impl Write, value: &impl Serialize, limit: usize) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    if body.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Данные действия слишком велики.",
        ));
    }
    sink.write_all(&[PROTOCOL])?;
    sink.write_all(&(body.len() as u32).to_le_bytes())?;
    sink.write_all(&body)?;
    sink.flush()
}

fn read_packet<T: DeserializeOwned>(mut source: impl Read, limit: usize) -> io::Result<T> {
    let mut header = [0u8; 5];
    source.read_exact(&mut header)?;
    let size = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if header[0] != PROTOCOL || size == 0 || size > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Некорректный ответ канала действия.",
        ));
    }
    // Consume only the framed response. Never wait for pipe EOF from a child
    // process the shell launched in addition to the helper.
    let mut body = vec![0; size];
    source.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Source;
    use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};

    #[test]
    fn action_protocol_preserves_elevation_and_stops_at_one_frame() {
        let mut bytes = Vec::new();
        for operation in [
            ShellOperation::Launch(false),
            ShellOperation::Launch(true),
            ShellOperation::Reveal,
        ] {
            write_packet(&mut bytes, &operation, MAX_REQUEST).unwrap();
        }
        let mut pipe = bytes.as_slice();
        assert_eq!(
            read_packet::<ShellOperation>(&mut pipe, MAX_REQUEST).unwrap(),
            ShellOperation::Launch(false)
        );
        assert_eq!(
            read_packet::<ShellOperation>(&mut pipe, MAX_REQUEST).unwrap(),
            ShellOperation::Launch(true)
        );
        assert_eq!(
            read_packet::<ShellOperation>(&mut pipe, MAX_REQUEST).unwrap(),
            ShellOperation::Reveal
        );
        assert!(pipe.is_empty());
    }

    #[test]
    fn action_protocol_rejects_unbounded_truncated_or_wrong_version_packets() {
        assert!(read_packet::<String>(&[PROTOCOL, 32, 0, 0, 0][..], 16).is_err());
        assert!(read_packet::<String>(&[PROTOCOL, 5, 0, 0, 0, b'"'][..], 16).is_err());
        assert!(read_packet::<String>(&[2, 1, 0, 0, 0, b'0'][..], 16).is_err());
        assert!(write_packet(Vec::new(), &"недопустимая длина", 4).is_err());
    }

    #[test]
    fn action_protocol_preserves_the_exact_windows_shortcut_path() {
        let mut path: Vec<u16> = r"C:Меню ПускРабота & учёба ".encode_utf16().collect();
        path.push(0xd800);
        path.extend(".lnk".encode_utf16());
        let request = ShellAction {
            operation: ShellOperation::Launch(true),
            name: "Тест приложения".into(),
            source: Source::Desktop(PathBuf::from(OsString::from_wide(&path))),
        };
        let mut bytes = Vec::new();
        write_packet(&mut bytes, &request, MAX_REQUEST).unwrap();
        let decoded: ShellAction = read_packet(bytes.as_slice(), MAX_REQUEST).unwrap();
        assert_eq!(decoded.source, request.source);
        assert_eq!(decoded.name, request.name);
        assert_eq!(decoded.operation, request.operation);
    }
}
