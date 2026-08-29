use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    },
};

pub struct KillJob(HANDLE);

impl KillJob {
    pub fn create() -> windows::core::Result<Self> {
        let handle = unsafe { CreateJobObjectW(None, None)? };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .expect("job limit structure size fits u32"),
            )?;
        }
        Ok(Self(handle))
    }

    pub fn assign_handle(&self, process: HANDLE) -> windows::core::Result<()> {
        unsafe { AssignProcessToJobObject(self.0, process) }
    }

    pub fn terminate(&self) -> windows::core::Result<()> {
        unsafe { TerminateJobObject(self.0, 1) }
    }
}

impl Drop for KillJob {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
