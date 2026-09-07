use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    mem::size_of,
    ptr,
};

use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, HANDLE, HLOCAL, LocalFree,
        },
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_STATISTICS,
            TOKEN_USER, TokenStatistics, TokenUser,
        },
        System::{
            Registry::{
                HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE,
                REG_OPTION_VOLATILE, REG_QWORD, REG_SZ, RegCloseKey, RegCreateKeyExW,
                RegDeleteTreeW, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
            },
            RemoteDesktop::{
                WTS_SESSION_INFOW, WTSActive, WTSConnected, WTSDisconnected, WTSEnumerateSessionsW,
                WTSFreeMemory, WTSIdle, WTSQueryUserToken,
            },
        },
    },
    core::{PCWSTR, PWSTR, w},
};

const REGISTRATION_ROOT: PCWSTR = w!(r"SOFTWARE\SUSM\Registrations");
const RUNTIME_ROOT: PCWSTR = w!(r"SOFTWARE\SUSM\Runtime\ManagerSessions");

pub struct RuntimeSession {
    pub manager_session_id: String,
    pub windows_session_id: u32,
    pub authentication_id: u64,
    pub controller_process_id: u32,
    pub controller_creation_time: u64,
}

pub struct ActiveSession {
    pub session_id: u32,
    pub authentication_id: u64,
    token: HANDLE,
}

pub struct ActiveSessions {
    pub sessions: BTreeMap<String, ActiveSession>,
    pub conflicts: BTreeSet<String>,
    pub logons: BTreeSet<(u32, u64)>,
}

impl ActiveSession {
    pub fn take_token(&mut self) -> HANDLE {
        std::mem::take(&mut self.token)
    }
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        if !self.token.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.token);
            }
        }
    }
}

pub fn add(sid: &str) -> windows::core::Result<()> {
    let path = wide(&format!(r"SOFTWARE\SUSM\Registrations\{sid}"));
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            None,
            PWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()?;
        let version = 1_u32.to_le_bytes();
        let result = RegSetValueExW(key, w!("FormatVersion"), None, REG_DWORD, Some(&version)).ok();
        let _ = RegCloseKey(key);
        result
    }
}

pub fn remove(sid: &str) -> windows::core::Result<()> {
    let path = wide(&format!(r"SOFTWARE\SUSM\Registrations\{sid}"));
    let result = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr())) };
    if result == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        result.ok()
    }
}

pub fn list() -> windows::core::Result<Vec<String>> {
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            REGISTRATION_ROOT,
            None,
            KEY_READ,
            &mut key,
        )
    };
    if opened == ERROR_FILE_NOT_FOUND {
        return Ok(Vec::new());
    }
    opened.ok()?;
    let key = RegistryKey(key);
    let mut names = Vec::new();
    let mut index = 0;
    loop {
        let mut buffer = vec![0_u16; 256];
        let mut length = u32::try_from(buffer.len()).expect("registry name buffer length fits u32");
        let result = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                Some(PWSTR(buffer.as_mut_ptr())),
                &mut length,
                None,
                None,
                None,
                None,
            )
        };
        if result == ERROR_NO_MORE_ITEMS {
            break;
        }
        result.ok()?;
        buffer.truncate(length as usize);
        names.push(String::from_utf16_lossy(&buffer));
        index += 1;
    }
    Ok(names)
}

pub fn save_runtime(sid: &str, session: &RuntimeSession) -> windows::core::Result<()> {
    let path = wide(&format!(r"SOFTWARE\SUSM\Runtime\ManagerSessions\{sid}"));
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            None,
            PWSTR::null(),
            REG_OPTION_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let key = RegistryKey(key);
    set_string(key.0, w!("ManagerSessionId"), &session.manager_session_id)?;
    set_dword(key.0, w!("WindowsSessionId"), session.windows_session_id)?;
    set_qword(key.0, w!("AuthenticationId"), session.authentication_id)?;
    set_dword(
        key.0,
        w!("ControllerProcessId"),
        session.controller_process_id,
    )?;
    set_qword(
        key.0,
        w!("ControllerCreationTime"),
        session.controller_creation_time,
    )
}

pub fn runtime_sessions() -> windows::core::Result<BTreeMap<String, RuntimeSession>> {
    let mut root = HKEY::default();
    let opened =
        unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, RUNTIME_ROOT, None, KEY_READ, &mut root) };
    if opened == ERROR_FILE_NOT_FOUND {
        return Ok(BTreeMap::new());
    }
    opened.ok()?;
    let root = RegistryKey(root);
    let mut sessions = BTreeMap::new();
    for sid in enum_subkeys(root.0)? {
        let path = wide(&sid);
        let mut key = HKEY::default();
        unsafe {
            RegOpenKeyExW(root.0, PCWSTR(path.as_ptr()), None, KEY_READ, &mut key).ok()?;
        }
        let key = RegistryKey(key);
        sessions.insert(
            sid,
            RuntimeSession {
                manager_session_id: query_string(key.0, w!("ManagerSessionId"))?,
                windows_session_id: query_dword(key.0, w!("WindowsSessionId"))?,
                authentication_id: query_qword(key.0, w!("AuthenticationId"))?,
                controller_process_id: query_dword(key.0, w!("ControllerProcessId"))?,
                controller_creation_time: query_qword(key.0, w!("ControllerCreationTime"))?,
            },
        );
    }
    Ok(sessions)
}

pub fn remove_runtime(sid: &str) -> windows::core::Result<()> {
    let path = wide(&format!(r"SOFTWARE\SUSM\Runtime\ManagerSessions\{sid}"));
    let result = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr())) };
    if result == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        result.ok()
    }
}

fn enum_subkeys(key: HKEY) -> windows::core::Result<Vec<String>> {
    let mut names = Vec::new();
    let mut index = 0;
    loop {
        let mut buffer = vec![0_u16; 256];
        let mut length = u32::try_from(buffer.len()).expect("registry name buffer length fits u32");
        let result = unsafe {
            RegEnumKeyExW(
                key,
                index,
                Some(PWSTR(buffer.as_mut_ptr())),
                &mut length,
                None,
                None,
                None,
                None,
            )
        };
        if result == ERROR_NO_MORE_ITEMS {
            break;
        }
        result.ok()?;
        buffer.truncate(length as usize);
        names.push(String::from_utf16_lossy(&buffer));
        index += 1;
    }
    Ok(names)
}

fn set_string(key: HKEY, name: PCWSTR, value: &str) -> windows::core::Result<()> {
    let value = value.encode_utf16().chain([0]).collect::<Vec<_>>();
    let bytes = unsafe {
        std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * size_of::<u16>())
    };
    unsafe { RegSetValueExW(key, name, None, REG_SZ, Some(bytes)).ok() }
}

fn set_dword(key: HKEY, name: PCWSTR, value: u32) -> windows::core::Result<()> {
    unsafe { RegSetValueExW(key, name, None, REG_DWORD, Some(&value.to_le_bytes())).ok() }
}

fn set_qword(key: HKEY, name: PCWSTR, value: u64) -> windows::core::Result<()> {
    unsafe { RegSetValueExW(key, name, None, REG_QWORD, Some(&value.to_le_bytes())).ok() }
}

fn query_string(key: HKEY, name: PCWSTR) -> windows::core::Result<String> {
    let bytes = query_value(key, name, REG_SZ)?;
    if bytes.len() % 2 != 0 {
        return Err(windows::core::Error::from_thread());
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .take_while(|value| *value != 0)
        .collect::<Vec<_>>();
    String::from_utf16(&wide).map_err(|_| windows::core::Error::from_thread())
}

fn query_dword(key: HKEY, name: PCWSTR) -> windows::core::Result<u32> {
    let bytes = query_value(key, name, REG_DWORD)?;
    let value: [u8; 4] = bytes
        .try_into()
        .map_err(|_| windows::core::Error::from_thread())?;
    Ok(u32::from_le_bytes(value))
}

fn query_qword(key: HKEY, name: PCWSTR) -> windows::core::Result<u64> {
    let bytes = query_value(key, name, REG_QWORD)?;
    let value: [u8; 8] = bytes
        .try_into()
        .map_err(|_| windows::core::Error::from_thread())?;
    Ok(u64::from_le_bytes(value))
}

fn query_value(
    key: HKEY,
    name: PCWSTR,
    expected_type: windows::Win32::System::Registry::REG_VALUE_TYPE,
) -> windows::core::Result<Vec<u8>> {
    let mut value_type = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
    let mut size = 0;
    unsafe {
        RegQueryValueExW(
            key,
            name,
            None,
            Some(&mut value_type),
            None,
            Some(&mut size),
        )
        .ok()?;
    }
    if value_type != expected_type {
        return Err(windows::core::Error::from_thread());
    }
    let mut value = vec![0_u8; size as usize];
    unsafe {
        RegQueryValueExW(
            key,
            name,
            None,
            Some(&mut value_type),
            Some(value.as_mut_ptr()),
            Some(&mut size),
        )
        .ok()?;
    }
    value.truncate(size as usize);
    Ok(value)
}

pub fn active_sessions() -> windows::core::Result<ActiveSessions> {
    let mut sessions = ptr::null_mut::<WTS_SESSION_INFOW>();
    let mut count = 0;
    unsafe {
        WTSEnumerateSessionsW(None, 0, 1, &mut sessions, &mut count)?;
    }
    let allocation = WtsAllocation(sessions.cast());
    let entries = unsafe { std::slice::from_raw_parts(sessions, count as usize) };
    let mut active = BTreeMap::new();
    let mut conflicts = BTreeSet::new();
    let mut logons = BTreeSet::new();
    for entry in entries.iter().filter(|entry| {
        entry.State == WTSActive
            || entry.State == WTSConnected
            || entry.State == WTSDisconnected
            || entry.State == WTSIdle
    }) {
        let mut token = HANDLE::default();
        if unsafe { WTSQueryUserToken(entry.SessionId, &mut token) }.is_err() {
            continue;
        }
        match token_sid(token) {
            Ok(sid) => {
                let authentication_id = match token_authentication_id(token) {
                    Ok(authentication_id) => authentication_id,
                    Err(_) => {
                        unsafe {
                            let _ = CloseHandle(token);
                        }
                        continue;
                    }
                };
                let session = ActiveSession {
                    session_id: entry.SessionId,
                    authentication_id,
                    token,
                };
                logons.insert((entry.SessionId, authentication_id));
                if conflicts.contains(&sid) {
                    drop(session);
                } else {
                    match active.entry(sid) {
                        Entry::Vacant(entry) => {
                            entry.insert(session);
                        }
                        Entry::Occupied(entry) => {
                            conflicts.insert(entry.key().clone());
                            drop(session);
                        }
                    }
                }
            }
            Err(_) => unsafe {
                let _ = CloseHandle(token);
            },
        }
    }
    drop(allocation);
    Ok(ActiveSessions {
        sessions: active,
        conflicts,
        logons,
    })
}

fn token_sid(token: HANDLE) -> windows::core::Result<String> {
    let mut required = 0;
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut required);
    }
    if required == 0 {
        return Err(windows::core::Error::from_thread());
    }
    let mut storage = vec![0_usize; (required as usize).div_ceil(size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(storage.as_mut_ptr().cast()),
            required,
            &mut required,
        )?;
    }
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    unsafe {
        ConvertSidToStringSidW(user.User.Sid, &mut text)?;
    }
    let value = unsafe { text.to_string() };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    }
    Ok(value?)
}

fn token_authentication_id(token: HANDLE) -> windows::core::Result<u64> {
    let mut statistics = TOKEN_STATISTICS::default();
    let mut required = 0;
    unsafe {
        GetTokenInformation(
            token,
            TokenStatistics,
            Some((&raw mut statistics).cast()),
            u32::try_from(size_of::<TOKEN_STATISTICS>()).expect("token statistics size fits u32"),
            &mut required,
        )?;
    }
    Ok(
        (u64::from(statistics.AuthenticationId.HighPart as u32) << 32)
            | u64::from(statistics.AuthenticationId.LowPart),
    )
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

struct WtsAllocation(*mut core::ffi::c_void);

impl Drop for WtsAllocation {
    fn drop(&mut self) {
        unsafe {
            WTSFreeMemory(self.0);
        }
    }
}
