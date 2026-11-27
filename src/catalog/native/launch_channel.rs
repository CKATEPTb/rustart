use std::{
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};

use serde::{Serialize, de::DeserializeOwned};
use windows_sys::Win32::{
    Foundation::{
        ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
        Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
        SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
        PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
            GetNamedPipeServerProcessId, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
        },
        Threading::{CreateEventW, WaitForSingleObject},
    },
};

use super::privilege;

const LIMIT: usize = 256 * 1024;
const DEADLINE: Duration = Duration::from_secs(15);

pub(super) struct Channel {
    handle: OwnedHandle,
    deadline: Instant,
}

impl Channel {
    pub(super) fn server(account: &privilege::Context) -> io::Result<(Self, String)> {
        let id = random_channel()?;
        let path = pipe_path(&id)?;
        let sddl: Vec<_> = format!("D:P(A;;GRGW;;;{})S:(ML;;NW;;;ME)", account.sid)
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor = null_mut();
        // The protected DACL grants access to the verified interactive SID only.
        // The explicit medium label lets a standard process write the response.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let handle = unsafe {
            CreateNamedPipeW(
                path.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                &security,
            )
        };
        let result = owned(handle);
        unsafe { LocalFree(descriptor.cast()) };
        Ok((Self::new(result?), id))
    }

    pub(super) fn client(id: &str) -> io::Result<Self> {
        let path = pipe_path(id)?;
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        Ok(Self::new(owned(handle)?))
    }

    fn new(handle: OwnedHandle) -> Self {
        Self {
            handle,
            deadline: Instant::now() + DEADLINE,
        }
    }

    pub(super) fn connect(&self) -> io::Result<()> {
        self.io(true, |handle, state| unsafe {
            ConnectNamedPipe(handle, state)
        })
        .map(|_| ())
    }

    pub(super) fn peer(&self, server: bool) -> io::Result<privilege::Process> {
        let mut pid = 0;
        let get_pid = if server {
            GetNamedPipeClientProcessId
        } else {
            GetNamedPipeServerProcessId
        };
        if unsafe { get_pid(self.handle.as_raw_handle(), &mut pid) } == 0 || pid == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = privilege::Process::open(pid)?;
        if !privilege::paths_equal(&process.image()?, &std::env::current_exe()?) {
            return Err(privilege::unverified(
                "the launch channel peer is not Rustart",
            ));
        }
        Ok(process)
    }

    pub(super) fn send(&self, object: &impl Serialize) -> io::Result<()> {
        let data = serde_json::to_vec(object).map_err(io::Error::other)?;
        if data.len() > LIMIT {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.write_all(&(data.len() as u32).to_le_bytes())?;
        self.write_all(&data)
    }

    pub(super) fn receive<T: DeserializeOwned>(&self) -> io::Result<T> {
        let mut len = [0; 4];
        self.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 || len > LIMIT {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut data = vec![0; len];
        self.read_exact(&mut data)?;
        serde_json::from_slice(&data).map_err(io::Error::other)
    }

    fn read_exact(&self, mut bytes: &mut [u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = self.io(false, |handle, state| unsafe {
                ReadFile(
                    handle,
                    bytes.as_mut_ptr(),
                    bytes.len() as u32,
                    null_mut(),
                    state,
                )
            })?;
            if count == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            bytes = &mut bytes[count..];
        }
        Ok(())
    }

    fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = self.io(false, |handle, state| unsafe {
                WriteFile(
                    handle,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    null_mut(),
                    state,
                )
            })?;
            if count == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[count..];
        }
        Ok(())
    }

    fn io(
        &self,
        connect: bool,
        operate: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
    ) -> io::Result<usize> {
        let pending_for = self.deadline.saturating_duration_since(Instant::now());
        if pending_for.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let event = owned(unsafe { CreateEventW(null(), 1, 0, null()) })?;
        let handle = self.handle.as_raw_handle();
        let mut state = OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..Default::default()
        };
        if operate(handle, &mut state) == 0 {
            let error = io::Error::last_os_error();
            let code = error.raw_os_error().map(|code| code as u32);
            if connect && code == Some(ERROR_PIPE_CONNECTED) {
                return Ok(0);
            }
            if code != Some(ERROR_IO_PENDING) {
                return Err(error);
            }
            let milliseconds = pending_for.as_millis() as u32 + 1;
            let waited = unsafe { WaitForSingleObject(event.as_raw_handle(), milliseconds) };
            if waited != WAIT_OBJECT_0 {
                let error = if waited == WAIT_TIMEOUT {
                    io::ErrorKind::TimedOut.into()
                } else {
                    io::Error::last_os_error()
                };
                // Complete cancellation before releasing the buffer and OVERLAPPED.
                let mut ignored = 0;
                unsafe {
                    CancelIoEx(handle, &state);
                    GetOverlappedResult(handle, &state, &mut ignored, 1);
                }
                return Err(error);
            }
        }
        let mut count = 0;
        if unsafe { GetOverlappedResult(handle, &state, &mut count, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(count as usize)
    }
}

fn owned(raw: HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

fn random_channel() -> io::Result<String> {
    let mut random = [0; 16];
    if unsafe {
        BCryptGenRandom(
            null_mut(),
            random.as_mut_ptr(),
            random.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    } < 0
    {
        return Err(io::Error::other(
            "could not generate a private channel identifier",
        ));
    }
    let hex = b"0123456789abcdef";
    let mut id = String::with_capacity(32);
    for byte in random {
        id.push(hex[(byte >> 4) as usize] as char);
        id.push(hex[(byte & 15) as usize] as char);
    }
    Ok(id)
}

fn pipe_path(id: &str) -> io::Result<Vec<u16>> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(format!(r"\\.\pipe\Rustart.StandardLaunch.{id}")
        .encode_utf16()
        .chain([0])
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_pipe_names_from_the_command_line() {
        assert!(pipe_path("0123456789abcdef0123456789abcdef").is_ok());
        assert!(pipe_path("0123456789ABCDEF0123456789ABCDEF").is_err());
        assert!(pipe_path(r"\\server\pipe\Rustart.StandardLaunch").is_err());
        assert!(pipe_path("0123456789abcdef0123456789abcdef ").is_err());
    }

    #[test]
    fn exchanges_framed_messages_over_a_local_pipe() {
        let account = privilege::current().unwrap();
        let (server, nonce) = Channel::server(&account).unwrap();
        let client = std::thread::spawn(move || {
            let client = Channel::client(&nonce).unwrap();
            assert_eq!(
                client.peer(false).unwrap().context().unwrap().session,
                account.session
            );
            assert_eq!(client.receive::<String>().unwrap(), "request");
            client.send(&"reply").unwrap();
        });
        server.connect().unwrap();
        assert!(server.peer(true).is_ok());
        server.send(&"request").unwrap();
        assert_eq!(server.receive::<String>().unwrap(), "reply");
        client.join().unwrap();
    }
}
