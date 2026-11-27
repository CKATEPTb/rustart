use std::{
    ffi::OsString,
    io,
    mem::size_of,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    slice,
};

use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE, LocalFree},
    Globalization::{CSTR_EQUAL, CompareStringOrdinal},
    Security::{
        Authorization::ConvertSidToStringSidW, GetLengthSid, GetSidSubAuthority,
        GetSidSubAuthorityCount, GetTokenInformation, IsValidSid, LookupAccountSidW,
        TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
        TokenElevation, TokenIntegrityLevel, TokenSessionId, TokenUser,
    },
    System::{
        RemoteDesktop::{
            ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_INFO_CLASS, WTSDomainName,
            WTSFreeMemory, WTSQuerySessionInformationW, WTSUserName,
        },
        SystemInformation::GetSystemWindowsDirectoryW,
        SystemServices::SECURITY_MANDATORY_MEDIUM_RID,
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    },
};

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Context {
    pub(super) session: u32,
    pub(super) sid: String,
    elevated: bool,
    integrity: u32,
}

impl Context {
    pub(super) fn standard(&self) -> bool {
        !self.elevated && self.integrity == SECURITY_MANDATORY_MEDIUM_RID as u32
    }

    pub(super) fn can_delegate(&self) -> bool {
        self.integrity >= SECURITY_MANDATORY_MEDIUM_RID as u32 && self.session != 0
    }
}

pub(super) fn current() -> io::Result<Context> {
    context(unsafe { GetCurrentProcess() })
}

pub(super) struct Process {
    handle: OwnedHandle,
    pid: u32,
}

impl Process {
    pub(super) fn open(pid: u32) -> io::Result<Self> {
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        Ok(Self {
            handle: owned(raw)?,
            pid,
        })
    }

    pub(super) fn context(&self) -> io::Result<Context> {
        context(self.handle.as_raw_handle())
    }

    pub(super) fn session(&self) -> io::Result<u32> {
        let mut session = 0;
        if unsafe { ProcessIdToSessionId(self.pid, &mut session) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(session)
    }

    pub(super) fn image(&self) -> io::Result<PathBuf> {
        let mut value = vec![0; 32768];
        let mut size = value.len() as u32;
        if unsafe {
            QueryFullProcessImageNameW(
                self.handle.as_raw_handle(),
                0,
                value.as_mut_ptr(),
                &mut size,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(PathBuf::from(OsString::from_wide(&value[..size as usize])))
    }

    pub(super) fn verify_interactive(&self, expected: &Context) -> io::Result<()> {
        let token = process_token(self.handle.as_raw_handle())?;
        let user = info(&token, TokenUser)?;
        let sid = token_sid(&user)?;
        let mut name = [0; 512];
        let mut domain = [0; 512];
        let mut name_size = name.len() as u32;
        let mut domain_size = domain.len() as u32;
        let mut kind = 0;
        if unsafe {
            LookupAccountSidW(
                null(),
                sid,
                name.as_mut_ptr(),
                &mut name_size,
                domain.as_mut_ptr(),
                &mut domain_size,
                &mut kind,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let actual_name = nul_wide(&name[..name_size as usize]);
        let actual_domain = nul_wide(&domain[..domain_size as usize]);
        let expected_name = session_account(expected.session, WTSUserName)?;
        let expected_domain = session_account(expected.session, WTSDomainName)?;
        if expected_name.is_empty()
            || !account_eq(&actual_name, &expected_name)
            || !account_eq(&actual_domain, &expected_domain)
        {
            return Err(unverified(
                "desktop shell account does not own the interactive session",
            ));
        }
        Ok(())
    }
}

pub(super) fn paths_equal(left: &Path, right: &Path) -> bool {
    account_eq(left.as_os_str(), right.as_os_str())
}

fn account_eq(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    if left == right {
        return true;
    }
    let left: Vec<_> = left.encode_wide().collect();
    let right: Vec<_> = right.encode_wide().collect();
    unsafe {
        CompareStringOrdinal(
            left.as_ptr(),
            left.len() as i32,
            right.as_ptr(),
            right.len() as i32,
            1,
        ) == CSTR_EQUAL
    }
}

fn nul_wide(wide: &[u16]) -> OsString {
    let end = wide
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(wide.len());
    OsString::from_wide(&wide[..end])
}

pub(super) fn explorer_image() -> io::Result<PathBuf> {
    let mut path = [0; 260];
    let length =
        unsafe { GetSystemWindowsDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    if length == 0 || length >= path.len() {
        return Err(unverified("could not locate the Windows system directory"));
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..length])).join("explorer.exe"))
}

pub(super) fn unverified(detail: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, detail)
}

fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn process_token(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut raw = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    owned(raw)
}

fn context(process: HANDLE) -> io::Result<Context> {
    let token = process_token(process)?;
    let elevation = scalar::<TOKEN_ELEVATION>(&token, TokenElevation)?;
    let session = scalar::<u32>(&token, TokenSessionId)?;
    let label = info(&token, TokenIntegrityLevel)?;
    let integrity = integrity_rid(&label)?;
    let sid = sid_of(&token)?;
    Ok(Context {
        session,
        sid,
        elevated: elevation.TokenIsElevated != 0,
        integrity,
    })
}

pub(crate) fn current_user_sid() -> io::Result<String> {
    let token = process_token(unsafe { GetCurrentProcess() })?;
    sid_of(&token)
}

fn sid_of(token: &OwnedHandle) -> io::Result<String> {
    let user = info(token, TokenUser)?;
    let sid = token_sid(&user)?;
    let mut raw = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut raw) } == 0 || raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let value = unsafe { windows::core::PCWSTR(raw).to_string() };
    unsafe { LocalFree(raw.cast()) };
    value.map_err(io::Error::other)
}

fn scalar<T: Default>(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> io::Result<T> {
    let mut value = T::default();
    let mut length = 0;
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            (&raw mut value).cast(),
            size_of::<T>() as u32,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != size_of::<T>() {
        return Err(unverified("unexpected token information size"));
    }
    Ok(value)
}

fn info(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
    let mut size = 0;
    unsafe { GetTokenInformation(token.as_raw_handle(), class, null_mut(), 0, &mut size) };
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) || size == 0 || size > 4096 {
        return Err(error);
    }
    let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(buffer)
}

fn token_sid(user: &[usize]) -> io::Result<*mut core::ffi::c_void> {
    if size_of_val(user) < size_of::<TOKEN_USER>() {
        return Err(unverified("missing token user"));
    }
    let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(unverified("invalid token user SID"));
    }
    Ok(sid)
}

fn integrity_rid(label: &[usize]) -> io::Result<u32> {
    if size_of_val(label) < size_of::<TOKEN_MANDATORY_LABEL>() {
        return Err(unverified("missing token integrity label"));
    }
    let sid = unsafe { (*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 || unsafe { GetLengthSid(sid) } < 12 {
        return Err(unverified("invalid token integrity SID"));
    }
    unsafe {
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return Err(unverified("missing token integrity RID"));
        }
        Ok(*GetSidSubAuthority(sid, u32::from(count - 1)))
    }
}

fn session_account(session: u32, class: WTS_INFO_CLASS) -> io::Result<OsString> {
    let mut buffer = null_mut();
    let mut size = 0;
    if unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session,
            class,
            &mut buffer,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if buffer.is_null() {
        return Err(unverified("missing interactive session account"));
    }
    let wide = unsafe { slice::from_raw_parts(buffer, size as usize / size_of::<u16>()) };
    let end = wide
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(wide.len());
    let value = OsString::from_wide(&wide[..end]);
    unsafe { WTSFreeMemory(buffer.cast()) };
    Ok(value)
}
