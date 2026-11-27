use std::{
    io::{self, BufReader, BufWriter, Read, Write},
    mem::size_of,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::Path,
    process::{Child, ChildStdout, Command, Stdio},
    ptr::null,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
        Threading::{CREATE_NO_WINDOW, WaitForSingleObject},
    },
};

use super::{App, discover};

const PROTOCOL: u32 = 3;
// Protective ceilings, with room for unusually large Start menus and slow
// folder redirection. A scan error leaves the previously published index intact.
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_APPS: usize = 128 * 1024;
const TIMEOUT: Duration = Duration::from_secs(75);

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Packet {
    version: u32,
    catalog: Result<Vec<App>, String>,
}

/// Internal child entry point. Stdout contains exactly one versioned packet.
pub fn write_discovery() -> Result<(), String> {
    let packet = Packet {
        version: PROTOCOL,
        catalog: discover().and_then(check_count),
    };
    let mut output = LimitedOutput {
        writer: BufWriter::new(io::stdout().lock()),
        remaining: MAX_BYTES,
    };
    serde_json::to_writer(&mut output, &packet)
        .map_err(|error| format!("Не удалось передать каталог приложений. {error}"))?;
    output
        .flush()
        .map_err(|error| format!("Не удалось передать каталог приложений. {error}"))
}

/// Index via a one-shot copy of this executable so shell state dies with it.
pub fn discover_isolated() -> Result<Vec<App>, String> {
    let executable = std::env::current_exe().map_err(worker_error)?;
    discover_from(&executable)
}

fn discover_from(executable: &Path) -> Result<Vec<App>, String> {
    // The job is private and non-inheritable. Closing this handle on a failed
    // scan or when Rustart exits also terminates a blocked child.
    let job = worker_job().map_err(worker_error)?;
    let mut child = Command::new(executable)
        .arg("--catalog-worker")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(worker_error)?;
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
        let error = io::Error::last_os_error();
        if !matches!(child.try_wait(), Ok(Some(_))) {
            let _ = child.kill();
            return Err(worker_error(error));
        }
    }
    let deadline = Instant::now() + TIMEOUT;
    let pipe = child
        .stdout
        .take()
        .ok_or("Не удалось открыть канал каталога.")?;
    let packet = read_packet(pipe, deadline)?;
    wait_for_success(&mut child, deadline)?;
    validate(packet)
}

fn worker_job() -> io::Result<OwnedHandle> {
    let raw = unsafe { CreateJobObjectW(null(), null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

fn read_packet(pipe: ChildStdout, deadline: Instant) -> Result<Packet, String> {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("rustart-catalog-pipe".into())
        .spawn(move || {
            let _ = tx.send(decode(pipe));
        })
        .map_err(worker_error)?;
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => timeout_error(),
            mpsc::RecvTimeoutError::Disconnected => "Чтение каталога было прервано.".into(),
        })?
}

fn wait_for_success(child: &mut Child, deadline: Instant) -> Result<(), String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let milliseconds = remaining.as_millis() as u32;
    let milliseconds = if remaining.is_zero() {
        0
    } else {
        milliseconds + 1
    };
    match unsafe { WaitForSingleObject(child.as_raw_handle(), milliseconds) } {
        WAIT_OBJECT_0 => {
            let status = child.wait().map_err(worker_error)?;
            if status.success() {
                return Ok(());
            }
            Err(format!("Сбор каталога завершился с ошибкой ({status})."))
        }
        WAIT_TIMEOUT => Err(timeout_error()),
        _ => Err(worker_error(io::Error::last_os_error())),
    }
}

fn timeout_error() -> String {
    "Истекло время ожидания каталога приложений.".into()
}

struct LimitedOutput<W> {
    writer: W,
    remaining: usize,
}

impl<W: Write> Write for LimitedOutput<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("catalog packet exceeds the output limit"));
        }
        let written = self.writer.write(bytes)?;
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

struct LimitedInput<R> {
    reader: R,
    remaining: usize,
}

impl<R: Read> Read for LimitedInput<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            // Check the real end of the pipe, rather than treating the limit as EOF.
            return if self.reader.read(&mut bytes[..1])? == 0 {
                Ok(0)
            } else {
                Err(io::ErrorKind::FileTooLarge.into())
            };
        }
        let limit = bytes.len().min(self.remaining);
        let count = self.reader.read(&mut bytes[..limit])?;
        self.remaining -= count;
        Ok(count)
    }
}

fn decode(reader: impl Read) -> Result<Packet, String> {
    let buffer = BufReader::new(LimitedInput {
        reader,
        remaining: MAX_BYTES,
    });
    // from_reader also checks for the real EOF after the one versioned object.
    serde_json::from_reader(buffer).map_err(|error| {
        if error.io_error_kind() == Some(io::ErrorKind::FileTooLarge) {
            "Дочерний каталог превышает допустимый размер.".into()
        } else {
            format!("Получен повреждённый каталог приложений. {error}")
        }
    })
}

fn validate(packet: Packet) -> Result<Vec<App>, String> {
    if packet.version != PROTOCOL {
        return Err("Неподдерживаемая версия каталога приложений.".into());
    }
    packet.catalog.and_then(check_count)
}

fn check_count(apps: Vec<App>) -> Result<Vec<App>, String> {
    if apps.len() > MAX_APPS {
        return Err("Каталог приложений превышает допустимое число записей.".into());
    }
    Ok(apps)
}

fn worker_error(error: io::Error) -> String {
    format!("Не удалось получить каталог приложений. {error}")
}

pub(super) mod windows_path {
    use std::{
        ffi::OsString,
        fmt,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
    };

    use serde::{
        Deserializer, Serializer,
        de::{Error, SeqAccess, Visitor},
    };

    pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        match path.to_str() {
            Some(text) => serializer.serialize_str(text),
            None => serializer.collect_seq(path.as_os_str().encode_wide()),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        struct WindowsPath;

        impl<'de> Visitor<'de> for WindowsPath {
            type Value = PathBuf;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Windows path as a string or UTF-16 units")
            }

            fn visit_str<E: Error>(self, path: &str) -> Result<PathBuf, E> {
                if path.is_empty() || path.contains('\0') {
                    return Err(E::custom("invalid Windows path"));
                }
                Ok(PathBuf::from(path))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<PathBuf, A::Error> {
                let mut wide = Vec::new();
                while let Some(unit) = sequence.next_element::<u16>()? {
                    if unit == 0 {
                        return Err(A::Error::custom("invalid Windows path"));
                    }
                    wide.push(unit);
                }
                if wide.is_empty() {
                    return Err(A::Error::custom("invalid Windows path"));
                }
                Ok(PathBuf::from(OsString::from_wide(&wide)))
            }
        }

        deserializer.deserialize_any(WindowsPath)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Source, identity::Identity, search};
    use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};

    #[test]
    fn preserves_grouping_and_original_windows_paths() {
        // A Windows filename may contain an unpaired UTF-16 code unit.
        let mut path: Vec<_> = r"C:\Apps\Editor".encode_utf16().collect();
        path.push(0xd800);
        path.extend(".lnk".encode_utf16());
        let source = Source::Desktop(PathBuf::from(OsString::from_wide(&path)));
        let mut editor = App::new("Demo editor".into(), Some("Tools".into()), source.clone());
        editor.identity = Some(Identity::LocalFile(8, [9; 16]));
        let mut viewer = App::new(
            "Demo viewer".into(),
            None,
            Source::Packaged("demo_abcdefghijklm!Viewer".into()),
        );
        viewer.identity = editor.identity.clone();
        let packet = Packet {
            version: PROTOCOL,
            catalog: Ok(vec![editor, viewer]),
        };
        let bytes = serde_json::to_vec(&packet).unwrap();

        let apps = validate(decode(bytes.as_slice()).unwrap()).unwrap();
        assert_eq!(apps[0].source, source);
        assert_eq!(apps, packet.catalog.unwrap());
        assert_eq!(search(&apps, "demo"), [0]);
        assert_eq!(search(&apps, "demo viewer"), [1]);
    }

    #[test]
    fn rejects_unversioned_truncated_or_extra_data_and_preserves_child_errors() {
        let child_error = serde_json::to_vec(&Packet {
            version: PROTOCOL,
            catalog: Err("Оболочка Windows недоступна.".into()),
        })
        .unwrap();
        assert_eq!(
            decode(child_error.as_slice()).and_then(validate),
            Err("Оболочка Windows недоступна.".into())
        );
        assert!(
            decode(&b"{\"version\":2,\"catalog\":{\"Ok\":[]}}"[..])
                .and_then(validate)
                .is_err()
        );
        assert!(decode(&child_error[..child_error.len() - 1]).is_err());
        assert!(decode(&b"{\"version\":3,\"catalog\":{\"Ok\":[]}}{}"[..]).is_err());
    }

    #[test]
    fn limits_both_pipe_input_and_output() {
        let mut input = LimitedInput {
            reader: &b"1234"[..],
            remaining: 3,
        };
        let mut bytes = [0; 3];
        input.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"123");
        assert_eq!(
            input.read(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        let mut exact = LimitedInput {
            reader: &b"123"[..],
            remaining: 3,
        };
        exact.read_exact(&mut bytes).unwrap();
        assert_eq!(exact.read(&mut bytes).unwrap(), 0);
        let mut output = LimitedOutput {
            writer: Vec::new(),
            remaining: 3,
        };
        output.write_all(b"abc").unwrap();
        assert!(output.write_all(b"d").is_err());
        assert_eq!(output.writer, b"abc");
    }

    #[test]
    fn ordinary_paths_use_compact_utf8_strings() {
        let source = Source::Desktop(PathBuf::from(r"C:\Программы\Editor.lnk"));
        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(json["Desktop"], r"C:\Программы\Editor.lnk");
        assert_eq!(serde_json::from_value::<Source>(json).unwrap(), source);
    }

    #[test]
    #[ignore = "requires a built Rustart executable in RUSTART_CATALOG_TEST_EXE"]
    fn scans_the_same_catalog_through_a_real_private_worker() {
        let executable = PathBuf::from(std::env::var_os("RUSTART_CATALOG_TEST_EXE").unwrap());
        let direct = discover().unwrap();
        let isolated = discover_from(&executable).unwrap();
        assert_eq!(isolated, direct);
        eprintln!(
            "Received {} catalog records through the worker pipe.",
            isolated.len()
        );
    }
}
