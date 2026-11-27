use std::{
    ffi::c_void,
    io::{self, BufReader, BufWriter},
    os::windows::{io::AsRawHandle, process::CommandExt},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{FILETIME, WAIT_OBJECT_0},
    System::Threading::{
        CREATE_NO_WINDOW, CloseThreadpoolTimer, CreateThreadpoolTimer, PTP_CALLBACK_INSTANCE,
        PTP_TIMER, SetThreadpoolTimer, TerminateProcess, WaitForSingleObject,
        WaitForThreadpoolTimerCallbacks,
    },
};

use super::{Icon, Key, shell, wire};

pub(super) fn serve() -> io::Result<()> {
    let mut input = BufReader::new(io::stdin().lock());
    let mut output = BufWriter::new(io::stdout().lock());
    let extractor = shell::Extractor::new();
    while let Some(key) = wire::read_key(&mut input)? {
        let icon = extractor
            .as_ref()
            .and_then(|shell| shell.get(&key.source, key.px));
        wire::write_icon(&mut output, icon.as_ref())?;
    }
    Ok(())
}

pub(super) struct Helper {
    child: Child,
    writer: Option<BufWriter<ChildStdin>>,
    reader: Option<BufReader<ChildStdout>>,
}

impl Helper {
    pub(super) fn start() -> io::Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("--icon-worker")
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let writer = child.stdin.take().map(BufWriter::new);
        let reader = child.stdout.take().map(BufReader::new);
        Ok(Self {
            child,
            writer,
            reader,
        })
    }

    pub(super) fn handle(&self) -> usize {
        self.child.as_raw_handle() as usize
    }

    pub(super) fn get(&mut self, key: &Key) -> io::Result<Option<Icon>> {
        // Terminating our own child unblocks either synchronous pipe operation.
        // The timer is drained before the underlying process handle can close.
        let _timeout = RequestTimeout::start(self.child.as_raw_handle())?;
        let writer = self.writer.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        let reader = self.reader.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        wire::write_key(writer, key)?;
        wire::read_icon(reader)
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        drop(self.writer.take()); // EOF tells the idle helper to uninitialize COM.
        drop(self.reader.take());
        let handle = self.child.as_raw_handle();
        if unsafe { WaitForSingleObject(handle, 250) } != WAIT_OBJECT_0 {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// The owner holds the shared worker lock while this borrowed handle is used.
pub(super) fn cancel(handle: usize) {
    unsafe { TerminateProcess(handle as *mut c_void, 1) };
}

struct RequestTimeout(PTP_TIMER);

impl RequestTimeout {
    fn start(handle: *mut c_void) -> io::Result<Self> {
        unsafe extern "system" fn expired(
            _: PTP_CALLBACK_INSTANCE,
            context: *mut c_void,
            _: PTP_TIMER,
        ) {
            unsafe { TerminateProcess(context, 1) };
        }
        let timer = unsafe { CreateThreadpoolTimer(Some(expired), handle, null()) };
        if timer == 0 {
            return Err(io::Error::last_os_error());
        }
        const REQUEST_TIME_100NS: i64 = -30_000_000;
        let due = FILETIME {
            dwLowDateTime: REQUEST_TIME_100NS as u32,
            dwHighDateTime: (REQUEST_TIME_100NS >> 32) as u32,
        };
        unsafe { SetThreadpoolTimer(timer, &due, 0, 0) };
        Ok(Self(timer))
    }
}

impl Drop for RequestTimeout {
    fn drop(&mut self) {
        unsafe {
            SetThreadpoolTimer(self.0, null_mut(), 0, 0);
            WaitForThreadpoolTimerCallbacks(self.0, 1);
            CloseThreadpoolTimer(self.0);
        }
    }
}
