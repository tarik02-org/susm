use std::{
    ffi::c_void,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    ptr,
    time::Duration,
};

use thiserror::Error;
use tokio::time::sleep;
use windows::{
    Win32::{
        Foundation::{HANDLE, HLOCAL, LocalFree, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        System::Threading::{
            CreateEventW, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
        },
    },
    core::PCWSTR,
};

use crate::pipe::UserSid;

#[derive(Debug, Error)]
pub enum SessionEventError {
    #[error("invalid manager-session id")]
    InvalidManagerSessionId,
    #[error("manager-session ending event failed: {0}")]
    Windows(#[from] windows::core::Error),
}

pub struct EndingEvent {
    handle: OwnedHandle,
    name: String,
}

impl EndingEvent {
    pub fn create(manager_session_id: &str, sid: &UserSid) -> Result<Self, SessionEventError> {
        let name = ending_event_name(manager_session_id)?;
        let sddl = format!("D:P(A;;GA;;;SY)(A;;0x00100000;;;{})", sid.as_str());
        let sddl = wide_null(&sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )?;
        }
        let descriptor = LocalDescriptor(descriptor.0);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                .expect("SECURITY_ATTRIBUTES size fits u32"),
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let event_name = wide_null(&name);
        let handle = unsafe {
            CreateEventW(
                Some(ptr::from_ref(&attributes)),
                true,
                false,
                PCWSTR(event_name.as_ptr()),
            )?
        };
        Ok(Self {
            handle: owned_handle(handle),
            name,
        })
    }

    pub fn open(manager_session_id: &str) -> Result<Self, SessionEventError> {
        let name = ending_event_name(manager_session_id)?;
        let event_name = wide_null(&name);
        let handle = unsafe {
            OpenEventW(
                SYNCHRONIZATION_SYNCHRONIZE,
                false,
                PCWSTR(event_name.as_ptr()),
            )?
        };
        Ok(Self {
            handle: owned_handle(handle),
            name,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn signal(&self) -> Result<(), SessionEventError> {
        unsafe { SetEvent(raw_handle(&self.handle))? };
        Ok(())
    }

    pub fn is_signaled(&self) -> Result<bool, SessionEventError> {
        let result = unsafe { WaitForSingleObject(raw_handle(&self.handle), 0) };
        if result == WAIT_OBJECT_0 {
            return Ok(true);
        }
        if result == WAIT_TIMEOUT {
            return Ok(false);
        }
        if result == WAIT_FAILED {
            return Err(windows::core::Error::from_thread().into());
        }
        Err(windows::core::Error::new(
            windows::Win32::Foundation::E_UNEXPECTED,
            format!("unexpected event wait result {}", result.0),
        )
        .into())
    }

    pub async fn wait(&self) -> Result<(), SessionEventError> {
        loop {
            let result = unsafe { WaitForSingleObject(raw_handle(&self.handle), 0) };
            if result == WAIT_OBJECT_0 {
                return Ok(());
            }
            if result == WAIT_FAILED {
                return Err(windows::core::Error::from_thread().into());
            }
            if result != WAIT_TIMEOUT {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_UNEXPECTED,
                    format!("unexpected event wait result {}", result.0),
                )
                .into());
            }
            sleep(Duration::from_millis(50)).await;
        }
    }
}

pub fn ending_event_name(manager_session_id: &str) -> Result<String, SessionEventError> {
    let valid = !manager_session_id.is_empty()
        && manager_session_id.len() <= 64
        && manager_session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if !valid {
        return Err(SessionEventError::InvalidManagerSessionId);
    }
    Ok(format!(
        r"Global\SUSM-manager-session-{manager_session_id}-ending"
    ))
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn owned_handle(handle: HANDLE) -> OwnedHandle {
    unsafe { OwnedHandle::from_raw_handle(handle.0 as RawHandle) }
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

struct LocalDescriptor(*mut c_void);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0)));
        }
    }
}
