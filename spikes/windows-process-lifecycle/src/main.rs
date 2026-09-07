use std::env;
use std::ffi::c_void;
use std::fs;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{TOKEN_DUPLICATE, TOKEN_QUERY};
use windows::Win32::System::Console::{
    CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, GetConsoleWindow, SetConsoleCtrlHandler,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CreateEventW, CreateProcessW,
    GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, PROCESS_SYNCHRONIZE, ResumeThread,
    STARTF_USESHOWWINDOW, STARTUPINFOW, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, SW_HIDE};
use windows::core::{BOOL, PCWSTR, PWSTR};

static BREAK_EVENT: AtomicIsize = AtomicIsize::new(0);

fn main() {
    let result = match env::args().nth(1).as_deref() {
        Some("--supervisor") => run_supervisor(),
        Some("--ctrl-workload") => run_ctrl_workload(),
        Some("--tree-parent") => run_tree_parent(),
        Some("--tree-child") => run_tree_child(),
        Some("--argv-check") => run_argv_check(),
        _ => run_root(),
    };

    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run_root() -> Result<(), String> {
    check_user_environment()?;

    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let process = create_process(
        &executable,
        &["--supervisor"],
        CREATE_NEW_CONSOLE,
        false,
        true,
    )?;

    wait_for(process.process.0, 15_000, "hidden supervisor")?;
    let exit_code = process.exit_code()?;
    if exit_code != 0 {
        return Err(format!(
            "hidden supervisor reported lifecycle check failure {exit_code}"
        ));
    }

    println!("ok fresh user environment block");
    println!("ok hidden private console");
    println!("ok suspended assignment before resume");
    println!("ok targeted CTRL_BREAK delivery");
    println!("ok Job Object descendant termination");
    println!("ok Windows argument round-trip");
    Ok(())
}

fn check_user_environment() -> Result<(), String> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE,
            &mut token,
        )
        .map_err(|error| error.to_string())?;
    }
    let token = OwnedHandle(token);

    let mut raw_environment = ptr::null_mut();
    unsafe {
        CreateEnvironmentBlock(&mut raw_environment, Some(token.0), false)
            .map_err(|error| error.to_string())?;
    }
    let environment = OwnedEnvironment(raw_environment);

    let expected_profile = env::var("USERPROFILE").map_err(|error| error.to_string())?;
    let mut cursor = environment.0.cast::<u16>();
    let mut actual_profile = None;
    loop {
        let mut length = 0;
        while unsafe { *cursor.add(length) } != 0 {
            length += 1;
        }
        if length == 0 {
            break;
        }

        let entry =
            std::ffi::OsString::from_wide(unsafe { std::slice::from_raw_parts(cursor, length) })
                .to_string_lossy()
                .into_owned();
        if let Some((name, value)) = entry.split_once('=')
            && name.eq_ignore_ascii_case("USERPROFILE")
        {
            actual_profile = Some(value.to_owned());
        }
        cursor = unsafe { cursor.add(length + 1) };
    }

    let actual_profile = actual_profile.ok_or("fresh environment omitted USERPROFILE")?;
    if !actual_profile.eq_ignore_ascii_case(&expected_profile) {
        return Err(format!(
            "fresh USERPROFILE {actual_profile:?} did not match process value {expected_profile:?}"
        ));
    }

    Ok(())
}

fn run_supervisor() -> Result<(), String> {
    let console_window = unsafe { GetConsoleWindow() };
    if console_window.0.is_null() {
        return Err("CREATE_NEW_CONSOLE did not give the supervisor a console".into());
    }
    if unsafe { IsWindowVisible(console_window).as_bool() } {
        return Err("the supervisor private console window is visible".into());
    }

    let job = OwnedHandle(
        unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(|error| error.to_string())?,
    );
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            ptr::from_ref(&limits).cast::<c_void>(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
    }

    check_ctrl_break(job.0)?;
    check_tree_termination(job.0)?;
    check_argument_encoding()?;
    Ok(())
}

fn check_ctrl_break(job: HANDLE) -> Result<(), String> {
    let process_id = unsafe { GetCurrentProcessId() };
    let ready_name = format!("Local\\susm-spike-{process_id}-ctrl-ready");
    let break_name = format!("Local\\susm-spike-{process_id}-ctrl-break");
    let ready = create_event(&ready_name)?;
    let _break_event = create_event(&break_name)?;
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let process = create_process(
        &executable,
        &["--ctrl-workload", &ready_name, &break_name],
        PROCESS_CREATION_FLAGS(CREATE_SUSPENDED.0 | CREATE_NEW_PROCESS_GROUP.0),
        false,
        false,
    )?;

    unsafe { AssignProcessToJobObject(job, process.process.0) }
        .map_err(|error| error.to_string())?;
    process.resume()?;
    wait_for(ready.0, 5_000, "CTRL_BREAK workload readiness")?;

    unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process.id) }
        .map_err(|error| error.to_string())?;
    wait_for(process.process.0, 5_000, "CTRL_BREAK workload exit")?;

    let exit_code = process.exit_code()?;
    if exit_code != 42 {
        return Err(format!(
            "CTRL_BREAK workload exited with {exit_code}, expected 42"
        ));
    }

    Ok(())
}

fn check_tree_termination(job: HANDLE) -> Result<(), String> {
    let process_id = unsafe { GetCurrentProcessId() };
    let ready_name = format!("Local\\susm-spike-{process_id}-tree-ready");
    let ready = create_event(&ready_name)?;
    let pid_file = env::temp_dir().join(format!("susm-spike-{process_id}-child.pid"));
    let pid_file_arg = pid_file.to_string_lossy().into_owned();
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let parent = create_process(
        &executable,
        &["--tree-parent", &ready_name, &pid_file_arg],
        CREATE_SUSPENDED,
        false,
        false,
    )?;

    unsafe { AssignProcessToJobObject(job, parent.process.0) }
        .map_err(|error| error.to_string())?;
    parent.resume()?;
    wait_for(ready.0, 5_000, "tree workload readiness")?;

    let child_id = fs::read_to_string(&pid_file)
        .map_err(|error| error.to_string())?
        .parse::<u32>()
        .map_err(|error| error.to_string())?;
    let child = OwnedHandle(
        unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, child_id) }
            .map_err(|error| error.to_string())?,
    );

    unsafe { TerminateJobObject(job, 99) }.map_err(|error| error.to_string())?;
    wait_for(parent.process.0, 5_000, "tree parent termination")?;
    wait_for(child.0, 5_000, "tree child termination")?;
    let _ = fs::remove_file(pid_file);
    Ok(())
}

fn run_ctrl_workload() -> Result<(), String> {
    let mut arguments = env::args().skip(2);
    let ready_name = arguments.next().ok_or("missing ready event name")?;
    let break_name = arguments.next().ok_or("missing break event name")?;
    let ready = create_event(&ready_name)?;
    let break_event = create_event(&break_name)?;

    BREAK_EVENT.store(break_event.0.0 as isize, Ordering::SeqCst);
    unsafe { SetConsoleCtrlHandler(Some(console_handler), true) }
        .map_err(|error| error.to_string())?;
    unsafe { SetEvent(ready.0) }.map_err(|error| error.to_string())?;

    wait_for(break_event.0, 10_000, "CTRL_BREAK handler")?;
    std::process::exit(42);
}

fn run_tree_parent() -> Result<(), String> {
    let mut arguments = env::args().skip(2);
    let ready_name = arguments.next().ok_or("missing tree ready event name")?;
    let pid_file = PathBuf::from(arguments.next().ok_or("missing child PID file")?);
    let ready = create_event(&ready_name)?;
    let child = Command::new(env::current_exe().map_err(|error| error.to_string())?)
        .arg("--tree-child")
        .spawn()
        .map_err(|error| error.to_string())?;

    fs::write(pid_file, child.id().to_string()).map_err(|error| error.to_string())?;
    unsafe { SetEvent(ready.0) }.map_err(|error| error.to_string())?;
    loop {
        std::thread::park();
    }
}

fn run_tree_child() -> Result<(), String> {
    loop {
        std::thread::park();
    }
}

const ARGUMENT_CASES: &[&str] = &[
    "",
    "plain",
    "with space",
    "trailing\\",
    "quote\"inside",
    "slashes\\\\\"quote",
];

fn check_argument_encoding() -> Result<(), String> {
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let mut arguments = vec!["--argv-check"];
    arguments.extend_from_slice(ARGUMENT_CASES);
    let process = create_process(
        &executable,
        &arguments,
        PROCESS_CREATION_FLAGS::default(),
        false,
        false,
    )?;
    wait_for(process.process.0, 5_000, "argument check")?;
    let exit_code = process.exit_code()?;
    if exit_code != 0 {
        return Err(format!("argument check exited with {exit_code}"));
    }
    Ok(())
}

fn run_argv_check() -> Result<(), String> {
    let actual = env::args().skip(2).collect::<Vec<_>>();
    if actual != ARGUMENT_CASES {
        return Err(format!("argument mismatch: {actual:?}"));
    }
    Ok(())
}

unsafe extern "system" fn console_handler(control_type: u32) -> BOOL {
    if control_type != CTRL_BREAK_EVENT {
        return BOOL(0);
    }

    let handle = BREAK_EVENT.load(Ordering::SeqCst);
    if handle != 0 {
        let _ = unsafe { SetEvent(HANDLE(handle as *mut c_void)) };
    }
    BOOL(1)
}

fn create_event(name: &str) -> Result<OwnedHandle, String> {
    let name = wide_null(name);
    let handle = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }
        .map_err(|error| error.to_string())?;
    Ok(OwnedHandle(handle))
}

fn create_process(
    executable: &Path,
    arguments: &[&str],
    flags: PROCESS_CREATION_FLAGS,
    inherit_handles: bool,
    hide_window: bool,
) -> Result<Process, String> {
    let executable_text = executable.to_string_lossy();
    let mut command_line = quote_windows_argument(&executable_text);
    for argument in arguments {
        command_line.push(' ');
        command_line.push_str(&quote_windows_argument(argument));
    }

    let application = wide_null(&executable_text);
    let mut command_line = wide_null(&command_line);
    let mut startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).map_err(|error| error.to_string())?,
        ..Default::default()
    };
    if hide_window {
        startup.dwFlags = STARTF_USESHOWWINDOW;
        startup.wShowWindow = SW_HIDE.0 as u16;
    }

    let mut information = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            inherit_handles,
            flags,
            None,
            PCWSTR::null(),
            &startup,
            &mut information,
        )
        .map_err(|error| error.to_string())?;
    }

    Ok(Process {
        process: OwnedHandle(information.hProcess),
        thread: OwnedHandle(information.hThread),
        id: information.dwProcessId,
    })
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

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn wait_for(handle: HANDLE, timeout_ms: u32, description: &str) -> Result<(), String> {
    let result = unsafe { WaitForSingleObject(handle, timeout_ms) };
    if result != WAIT_OBJECT_0 {
        return Err(format!("timed out waiting for {description}: {result:?}"));
    }
    Ok(())
}

struct Process {
    process: OwnedHandle,
    thread: OwnedHandle,
    id: u32,
}

impl Process {
    fn resume(&self) -> Result<(), String> {
        let previous_count = unsafe { ResumeThread(self.thread.0) };
        if previous_count == u32::MAX {
            return Err(windows::core::Error::from_thread().to_string());
        }
        Ok(())
    }

    fn exit_code(&self) -> Result<u32, String> {
        let mut exit_code = 0;
        unsafe { GetExitCodeProcess(self.process.0, &mut exit_code) }
            .map_err(|error| error.to_string())?;
        Ok(exit_code)
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

struct OwnedEnvironment(*mut c_void);

impl Drop for OwnedEnvironment {
    fn drop(&mut self) {
        let _ = unsafe { DestroyEnvironmentBlock(self.0) };
    }
}
