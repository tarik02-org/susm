use std::{
    collections::BTreeMap,
    ffi::c_void,
    fs::File,
    mem::{size_of, size_of_val},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    path::Path,
    ptr,
    time::Duration,
};

use thiserror::Error;
use tokio::time::sleep;
use windows::{
    Win32::{
        Foundation::{
            GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS,
            SetHandleInformation, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::{SECURITY_ATTRIBUTES, TOKEN_DUPLICATE, TOKEN_QUERY},
        Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING},
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            Pipes::CreatePipe,
            Threading::{
                CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
                CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
                GetCurrentProcess, GetExitCodeProcess, InitializeProcThreadAttributeList,
                LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
                STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
            },
        },
    },
    core::{BOOL, PCWSTR, PWSTR, w},
};

use crate::job::KillJob;

const MAX_COMMAND_LINE_CODE_UNITS: usize = 32_767;

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("invalid process specification: {0}")]
    Invalid(String),
    #[error("Windows process operation failed: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("process wait returned unexpected status {0}")]
    Wait(u32),
}

#[derive(Clone, Copy)]
pub enum OutputMode {
    Piped,
    Discard,
}

pub struct ProcessSpec<'a> {
    pub executable: &'a Path,
    pub arguments: &'a [String],
    pub working_directory: &'a Path,
    pub environment: &'a BTreeMap<String, String>,
    pub output: OutputMode,
    pub extra_creation_flags: PROCESS_CREATION_FLAGS,
}

pub struct WindowsChild {
    process: OwnedHandle,
    process_id: u32,
    stdout: Option<tokio::fs::File>,
    stderr: Option<tokio::fs::File>,
}

impl WindowsChild {
    pub fn spawn(spec: ProcessSpec<'_>, job: &KillJob) -> Result<Self, ProcessError> {
        let executable = spec.executable.to_str().ok_or_else(|| {
            ProcessError::Invalid("executable path is not valid Unicode".to_owned())
        })?;
        let working_directory = spec.working_directory.to_str().ok_or_else(|| {
            ProcessError::Invalid("working directory is not valid Unicode".to_owned())
        })?;
        let application = wide_null(executable)?;
        let command_line = encode_command_line(spec.executable, spec.arguments)?;
        let mut command_line = wide_null(&command_line)?;
        let current_directory = wide_null(working_directory)?;
        let environment = encode_environment(spec.environment)?;
        let io = ChildIo::create(spec.output)?;
        let inherited = [
            raw_handle(&io.stdin),
            raw_handle(&io.stdout_child),
            raw_handle(&io.stderr_child),
        ];
        let mut attributes = AttributeList::with_handle_list(&inherited)?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>())
            .map_err(|_| ProcessError::Invalid("STARTUPINFOEXW is too large".to_owned()))?;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = inherited[0];
        startup.StartupInfo.hStdOutput = inherited[1];
        startup.StartupInfo.hStdError = inherited[2];
        startup.lpAttributeList = attributes.pointer();

        let flags = PROCESS_CREATION_FLAGS(
            CREATE_SUSPENDED.0
                | CREATE_NEW_PROCESS_GROUP.0
                | CREATE_UNICODE_ENVIRONMENT.0
                | EXTENDED_STARTUPINFO_PRESENT.0
                | spec.extra_creation_flags.0,
        );
        let mut information = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(
                PCWSTR(application.as_ptr()),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                Some(environment.as_ptr().cast::<c_void>()),
                PCWSTR(current_directory.as_ptr()),
                ptr::from_ref(&startup.StartupInfo),
                &mut information,
            )?;
        }

        let process = owned_handle(information.hProcess);
        let thread = owned_handle(information.hThread);
        if let Err(error) = job.assign_handle(raw_handle(&process)) {
            let _ = unsafe { TerminateProcess(raw_handle(&process), 1) };
            return Err(error.into());
        }
        let previous_suspend_count = unsafe { ResumeThread(raw_handle(&thread)) };
        if previous_suspend_count == u32::MAX {
            let error = windows::core::Error::from_thread();
            let _ = unsafe { TerminateProcess(raw_handle(&process), 1) };
            return Err(error.into());
        }

        let (stdout, stderr) = io.into_parent_streams();
        Ok(Self {
            process,
            process_id: information.dwProcessId,
            stdout,
            stderr,
        })
    }

    pub fn id(&self) -> u32 {
        self.process_id
    }

    pub fn take_stdout(&mut self) -> Option<tokio::fs::File> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<tokio::fs::File> {
        self.stderr.take()
    }

    pub async fn wait(&self) -> Result<u32, ProcessError> {
        loop {
            let wait = unsafe { WaitForSingleObject(raw_handle(&self.process), 0) };
            if wait == WAIT_OBJECT_0 {
                let mut exit_code = 0;
                unsafe { GetExitCodeProcess(raw_handle(&self.process), &mut exit_code)? };
                return Ok(exit_code);
            }
            if wait == WAIT_FAILED {
                return Err(windows::core::Error::from_thread().into());
            }
            if wait != WAIT_TIMEOUT {
                return Err(ProcessError::Wait(wait.0));
            }
            sleep(Duration::from_millis(20)).await;
        }
    }
}

pub fn fresh_environment() -> Result<BTreeMap<String, String>, ProcessError> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE,
            &mut token,
        )?;
    }
    let token = owned_handle(token);
    let mut raw_environment = ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut raw_environment, Some(raw_handle(&token)), false)? };
    let environment = EnvironmentBlock(raw_environment);
    parse_environment_block(environment.0.cast::<u16>())
}

fn parse_environment_block(
    mut cursor: *const u16,
) -> Result<BTreeMap<String, String>, ProcessError> {
    let mut environment = BTreeMap::new();
    loop {
        let mut length = 0;
        while unsafe { *cursor.add(length) } != 0 {
            length += 1;
        }
        if length == 0 {
            break;
        }
        let entry = String::from_utf16(unsafe { std::slice::from_raw_parts(cursor, length) })
            .map_err(|_| {
                ProcessError::Invalid("user environment contains invalid UTF-16".to_owned())
            })?;
        let delimiter = if let Some(entry) = entry.strip_prefix('=') {
            entry.find('=').map(|index| index + 1)
        } else {
            entry.find('=')
        }
        .ok_or_else(|| ProcessError::Invalid("user environment entry has no '='".to_owned()))?;
        let (name, value) = entry.split_at(delimiter);
        environment.insert(name.to_uppercase(), value[1..].to_owned());
        cursor = unsafe { cursor.add(length + 1) };
    }
    Ok(environment)
}

fn encode_command_line(executable: &Path, arguments: &[String]) -> Result<String, ProcessError> {
    let executable = executable
        .to_str()
        .ok_or_else(|| ProcessError::Invalid("executable path is not valid Unicode".to_owned()))?;
    let mut command_line = quote_windows_argument(executable);
    for argument in arguments {
        command_line.push(' ');
        command_line.push_str(&quote_windows_argument(argument));
    }
    let length = command_line.encode_utf16().count() + 1;
    if length > MAX_COMMAND_LINE_CODE_UNITS {
        return Err(ProcessError::Invalid(format!(
            "encoded command line uses {length} UTF-16 code units, maximum is {MAX_COMMAND_LINE_CODE_UNITS}"
        )));
    }
    Ok(command_line)
}

fn encode_environment(environment: &BTreeMap<String, String>) -> Result<Vec<u16>, ProcessError> {
    let mut block = Vec::new();
    for (name, value) in environment {
        if name.is_empty() || name.contains('\0') || value.contains('\0') {
            return Err(ProcessError::Invalid(
                "environment names and values must not contain NUL".to_owned(),
            ));
        }
        if name.strip_prefix('=').unwrap_or(name).contains('=') {
            return Err(ProcessError::Invalid(format!(
                "environment name contains '=': {name}"
            )));
        }
        block.extend(format!("{name}={value}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(block)
}

fn quote_windows_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
    {
        return argument.to_owned();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in argument.chars() {
        if character == '\\' {
            backslashes += 1;
            continue;
        }
        if character == '"' {
            quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
            quoted.push('"');
        } else {
            quoted.extend(std::iter::repeat_n('\\', backslashes));
            quoted.push(character);
        }
        backslashes = 0;
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

fn wide_null(value: &str) -> Result<Vec<u16>, ProcessError> {
    if value.contains('\0') {
        return Err(ProcessError::Invalid("string contains NUL".to_owned()));
    }
    Ok(value.encode_utf16().chain([0]).collect())
}

struct ChildIo {
    stdin: OwnedHandle,
    stdout_child: OwnedHandle,
    stderr_child: OwnedHandle,
    stdout_parent: Option<OwnedHandle>,
    stderr_parent: Option<OwnedHandle>,
}

impl ChildIo {
    fn create(output: OutputMode) -> Result<Self, ProcessError> {
        let stdin = open_inherited_null(GENERIC_READ.0)?;
        match output {
            OutputMode::Piped => {
                let (stdout_parent, stdout_child) = inherited_pipe()?;
                let (stderr_parent, stderr_child) = inherited_pipe()?;
                Ok(Self {
                    stdin,
                    stdout_child,
                    stderr_child,
                    stdout_parent: Some(stdout_parent),
                    stderr_parent: Some(stderr_parent),
                })
            }
            OutputMode::Discard => Ok(Self {
                stdin,
                stdout_child: open_inherited_null(GENERIC_WRITE.0)?,
                stderr_child: open_inherited_null(GENERIC_WRITE.0)?,
                stdout_parent: None,
                stderr_parent: None,
            }),
        }
    }

    fn into_parent_streams(mut self) -> (Option<tokio::fs::File>, Option<tokio::fs::File>) {
        let stdout = self.stdout_parent.take().map(into_tokio_file);
        let stderr = self.stderr_parent.take().map(into_tokio_file);
        (stdout, stderr)
    }
}

fn inherited_pipe() -> Result<(OwnedHandle, OwnedHandle), ProcessError> {
    let attributes = inheritable_security_attributes();
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe { CreatePipe(&mut read, &mut write, Some(ptr::from_ref(&attributes)), 0)? };
    let read = owned_handle(read);
    let write = owned_handle(write);
    unsafe {
        SetHandleInformation(raw_handle(&read), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
    }
    Ok((read, write))
}

fn open_inherited_null(access: u32) -> Result<OwnedHandle, ProcessError> {
    let attributes = inheritable_security_attributes();
    let handle = unsafe {
        CreateFileW(
            w!("NUL"),
            access,
            FILE_SHARE_MODE::default(),
            Some(ptr::from_ref(&attributes)),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )?
    };
    Ok(owned_handle(handle))
}

fn inheritable_security_attributes() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES size fits u32"),
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: BOOL(1),
    }
}

struct AttributeList {
    storage: Vec<usize>,
    initialized: bool,
}

impl AttributeList {
    fn with_handle_list(handles: &[HANDLE]) -> Result<Self, ProcessError> {
        let mut bytes = 0;
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut bytes) };
        if bytes == 0 {
            return Err(windows::core::Error::from_thread().into());
        }
        let words = bytes.div_ceil(size_of::<usize>());
        let mut list = Self {
            storage: vec![0; words],
            initialized: false,
        };
        unsafe {
            InitializeProcThreadAttributeList(Some(list.pointer()), 1, None, &mut bytes)?;
            list.initialized = true;
            UpdateProcThreadAttribute(
                list.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                size_of_val(handles),
                None,
                None,
            )?;
        }
        Ok(list)
    }

    fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.storage.as_mut_ptr().cast())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { DeleteProcThreadAttributeList(self.pointer()) };
        }
    }
}

struct EnvironmentBlock(*mut c_void);

impl Drop for EnvironmentBlock {
    fn drop(&mut self) {
        let _ = unsafe { DestroyEnvironmentBlock(self.0) };
    }
}

fn owned_handle(handle: HANDLE) -> OwnedHandle {
    unsafe { OwnedHandle::from_raw_handle(handle.0 as RawHandle) }
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

fn into_tokio_file(handle: OwnedHandle) -> tokio::fs::File {
    tokio::fs::File::from_std(File::from(handle))
}
