use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use windows::{
    Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0},
        Security::{DuplicateTokenEx, SecurityImpersonation, TOKEN_ALL_ACCESS, TokenPrimary},
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{
                CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
                GetProcessTimes, OpenProcess, PROCESS_INFORMATION,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
                STARTUPINFOW, TerminateProcess, WaitForSingleObject,
            },
        },
    },
    core::{PCWSTR, PWSTR},
};

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("Windows controller launch operation failed: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("the registering user's environment has no LOCALAPPDATA")]
    LocalAppDataMissing,
    #[error("the installed controller does not exist: {0}")]
    ControllerMissing(PathBuf),
    #[error("cannot read selected user installation at {path}: {source}")]
    InstallationIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("selected user installation is invalid: {0}")]
    InvalidInstallation(String),
    #[error("selected user installation metadata is invalid: {0}")]
    InstallationToml(#[from] toml::de::Error),
    #[error("selected user installation version is invalid: {0}")]
    InstallationVersion(#[from] semver::Error),
    #[error("controller worker thread could not be started: {0}")]
    Thread(#[from] std::io::Error),
}

pub enum RunnerCommand {
    Restart,
    EndSession,
    Detach,
}

pub struct ControllerRunner {
    commands: mpsc::Sender<RunnerCommand>,
    process_identity: Arc<Mutex<Option<ProcessIdentity>>>,
    thread: Option<thread::JoinHandle<()>>,
}

#[derive(Clone, Copy)]
pub struct ProcessIdentity {
    pub process_id: u32,
    pub creation_time: u64,
}

pub type ProcessObserver = Arc<dyn Fn(Option<ProcessIdentity>) + Send + Sync>;

impl ControllerRunner {
    pub fn from_session_token(
        token: HANDLE,
        session_id: u32,
        manager_session_id: String,
        observer: ProcessObserver,
    ) -> Result<Self, LaunchError> {
        Self::from_token(
            OwnedHandle(token),
            session_id,
            manager_session_id,
            None,
            observer,
        )
    }

    pub fn adopt_from_session_token(
        token: HANDLE,
        session_id: u32,
        manager_session_id: String,
        process_identity: ProcessIdentity,
        observer: ProcessObserver,
    ) -> Result<Self, LaunchError> {
        Self::from_token(
            OwnedHandle(token),
            session_id,
            manager_session_id,
            Some(process_identity),
            observer,
        )
    }

    fn from_token(
        token: OwnedHandle,
        identity: u32,
        manager_session_id: String,
        adopted: Option<ProcessIdentity>,
        observer: ProcessObserver,
    ) -> Result<Self, LaunchError> {
        let mut primary = HANDLE::default();
        unsafe {
            DuplicateTokenEx(
                token.0,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            )?;
        }
        let primary = OwnedHandle(primary);
        let local_app_data = user_local_app_data(&primary)?;
        let initial =
            adopted.and_then(|process_identity| open_existing(process_identity, identity).ok());
        let initial = match initial {
            Some(process) => Some(process),
            None => {
                let controller = selected_controller(&local_app_data)?;
                launch(&primary, &controller, &manager_session_id).ok()
            }
        };
        let (commands, receiver) = mpsc::channel();
        let process_identity =
            Arc::new(Mutex::new(initial.as_ref().map(|(_, identity)| *identity)));
        let visible_process_identity = process_identity.clone();
        observer(initial.as_ref().map(|(_, identity)| *identity));
        let thread = thread::Builder::new()
            .name(format!("susm-controller-{identity}"))
            .spawn(move || {
                run_controller(
                    primary,
                    local_app_data,
                    manager_session_id,
                    receiver,
                    process_identity,
                    initial,
                    observer,
                );
            })?;
        Ok(Self {
            commands,
            process_identity: visible_process_identity,
            thread: Some(thread),
        })
    }

    pub fn process_id(&self) -> u32 {
        self.process_identity()
            .map_or(0, |identity| identity.process_id)
    }

    pub fn process_identity(&self) -> Option<ProcessIdentity> {
        *self
            .process_identity
            .lock()
            .expect("controller process identity lock poisoned")
    }

    pub fn restart(&self) {
        let _ = self.commands.send(RunnerCommand::Restart);
    }

    pub fn end_session(mut self) {
        let _ = self.commands.send(RunnerCommand::EndSession);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    pub fn detach(mut self) {
        let _ = self.commands.send(RunnerCommand::Detach);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_CONTROLLER_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    bundle_format: u32,
    version: String,
    manifest_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    bundle_format: u32,
    version: String,
    target: String,
    protocol_major: u32,
    controller_schema_read_min: u32,
    controller_schema_read_max: u32,
    controller_schema_write: u32,
    supervisor_runtime_formats: Vec<u32>,
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    size: u64,
    sha256: String,
}

fn selected_controller(local_app_data: &Path) -> Result<PathBuf, LaunchError> {
    let installation = local_app_data.join("Programs").join("susm");
    let selection_path = installation.join("current.susm-install");
    let selection_bytes = bounded_read(&selection_path, MAX_MANIFEST_BYTES)?;
    let selection: Selection =
        toml::from_str(std::str::from_utf8(&selection_bytes).map_err(|_| {
            LaunchError::InvalidInstallation("selection is not valid UTF-8".to_owned())
        })?)?;
    if selection.bundle_format != 1 {
        return Err(LaunchError::InvalidInstallation(
            "selection bundle_format must be 1".to_owned(),
        ));
    }
    let version = selection.version.parse::<semver::Version>()?;
    if !version.build.is_empty() || !valid_digest(&selection.manifest_sha256) {
        return Err(LaunchError::InvalidInstallation(
            "selection version or manifest identity is invalid".to_owned(),
        ));
    }
    let version_directory = installation.join("versions").join(format!(
        "{}-{}",
        selection.version, selection.manifest_sha256
    ));
    reject_reparse_point(&version_directory)?;
    let manifest_path = version_directory.join("manifest.toml");
    let manifest_bytes = bounded_read(&manifest_path, MAX_MANIFEST_BYTES)?;
    if hex(&Sha256::digest(&manifest_bytes)) != selection.manifest_sha256 {
        return Err(LaunchError::InvalidInstallation(
            "selected manifest digest does not match the selection".to_owned(),
        ));
    }
    let manifest: Manifest =
        toml::from_str(std::str::from_utf8(&manifest_bytes).map_err(|_| {
            LaunchError::InvalidInstallation("manifest is not valid UTF-8".to_owned())
        })?)?;
    validate_manifest(&manifest, &selection)?;
    let declared = manifest
        .files
        .iter()
        .find(|file| file.path == "susmd.exe")
        .ok_or_else(|| {
            LaunchError::InvalidInstallation("manifest does not declare susmd.exe".to_owned())
        })?;
    if declared.size > MAX_CONTROLLER_BYTES || !valid_digest(&declared.sha256) {
        return Err(LaunchError::InvalidInstallation(
            "susmd.exe manifest entry is invalid".to_owned(),
        ));
    }
    let controller = version_directory.join("susmd.exe");
    reject_reparse_point(&controller)?;
    let metadata =
        fs::metadata(&controller).map_err(|source| installation_io(&controller, source))?;
    if !metadata.is_file() || metadata.len() != declared.size {
        return Err(LaunchError::ControllerMissing(controller));
    }
    if digest_file(&controller)? != declared.sha256 {
        return Err(LaunchError::InvalidInstallation(
            "susmd.exe digest does not match its manifest".to_owned(),
        ));
    }
    Ok(controller)
}

fn validate_manifest(manifest: &Manifest, selection: &Selection) -> Result<(), LaunchError> {
    let expected_target = match std::env::consts::ARCH {
        "x86_64" => "x86_64-pc-windows-msvc",
        "aarch64" => "aarch64-pc-windows-msvc",
        value => {
            return Err(LaunchError::InvalidInstallation(format!(
                "unsupported native architecture {value}"
            )));
        }
    };
    if manifest.bundle_format != 1
        || manifest.protocol_major != 1
        || manifest.version != selection.version
        || manifest.target != expected_target
        || manifest.controller_schema_read_min > manifest.controller_schema_read_max
        || !(manifest.controller_schema_read_min..=manifest.controller_schema_read_max)
            .contains(&manifest.controller_schema_write)
        || !manifest.supervisor_runtime_formats.contains(&1)
    {
        return Err(LaunchError::InvalidInstallation(
            "selected manifest is incompatible".to_owned(),
        ));
    }
    Ok(())
}

fn bounded_read(path: &Path, maximum: u64) -> Result<Vec<u8>, LaunchError> {
    let metadata = fs::metadata(path).map_err(|source| installation_io(path, source))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(LaunchError::InvalidInstallation(format!(
            "{} is not a bounded regular file",
            path.display()
        )));
    }
    fs::read(path).map_err(|source| installation_io(path, source))
}

fn reject_reparse_point(path: &Path) -> Result<(), LaunchError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| installation_io(path, source))?;
    if metadata.file_type().is_symlink() {
        Err(LaunchError::InvalidInstallation(format!(
            "{} is a reparse point",
            path.display()
        )))
    } else {
        Ok(())
    }
}

fn digest_file(path: &Path) -> Result<String, LaunchError> {
    let mut file = fs::File::open(path).map_err(|source| installation_io(path, source))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| installation_io(path, source))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex(&digest.finalize()))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn installation_io(path: &Path, source: io::Error) -> LaunchError {
    LaunchError::InstallationIo {
        path: path.to_owned(),
        source,
    }
}

impl Drop for ControllerRunner {
    fn drop(&mut self) {
        let _ = self.commands.send(RunnerCommand::Detach);
    }
}

fn run_controller(
    token: OwnedHandle,
    local_app_data: PathBuf,
    manager_session_id: String,
    commands: mpsc::Receiver<RunnerCommand>,
    process_identity: Arc<Mutex<Option<ProcessIdentity>>>,
    mut initial: Option<(OwnedHandle, ProcessIdentity)>,
    observer: ProcessObserver,
) {
    let mut delay = Duration::from_millis(250);
    loop {
        let process = match initial.take().map(Ok).unwrap_or_else(|| {
            let controller = selected_controller(&local_app_data)?;
            launch(&token, &controller, &manager_session_id).map_err(LaunchError::from)
        }) {
            Ok(process) => {
                *process_identity
                    .lock()
                    .expect("controller process identity lock poisoned") = Some(process.1);
                observer(Some(process.1));
                process.0
            }
            Err(_) => {
                match commands.recv_timeout(delay) {
                    Ok(RunnerCommand::Restart) => delay = Duration::from_millis(250),
                    Ok(RunnerCommand::EndSession | RunnerCommand::Detach)
                    | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        delay = (delay * 2).min(Duration::from_secs(30));
                    }
                }
                continue;
            }
        };
        let launched_at = Instant::now();
        let mut immediate_restart = false;
        loop {
            match commands.try_recv() {
                Ok(RunnerCommand::EndSession) => {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while Instant::now() < deadline {
                        if unsafe { WaitForSingleObject(process.0, 250) } == WAIT_OBJECT_0 {
                            *process_identity
                                .lock()
                                .expect("controller process identity lock poisoned") = None;
                            observer(None);
                            return;
                        }
                    }
                    let _ = unsafe { TerminateProcess(process.0, 1) };
                    let _ = unsafe { WaitForSingleObject(process.0, 5_000) };
                    *process_identity
                        .lock()
                        .expect("controller process identity lock poisoned") = None;
                    observer(None);
                    return;
                }
                Ok(RunnerCommand::Detach) => return,
                Ok(RunnerCommand::Restart) => {
                    let _ = unsafe { TerminateProcess(process.0, 1) };
                    let _ = unsafe { WaitForSingleObject(process.0, 5_000) };
                    delay = Duration::from_millis(250);
                    immediate_restart = true;
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if unsafe { WaitForSingleObject(process.0, 250) } == WAIT_OBJECT_0 {
                break;
            }
        }
        *process_identity
            .lock()
            .expect("controller process identity lock poisoned") = None;
        observer(None);
        if immediate_restart {
            continue;
        }
        if launched_at.elapsed() >= Duration::from_secs(300) {
            delay = Duration::from_millis(250);
        }
        match commands.recv_timeout(delay) {
            Ok(RunnerCommand::Restart) => delay = Duration::from_millis(250),
            Ok(RunnerCommand::EndSession | RunnerCommand::Detach)
            | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
}

fn launch(
    token: &OwnedHandle,
    controller: &Path,
    manager_session_id: &str,
) -> windows::core::Result<(OwnedHandle, ProcessIdentity)> {
    let application = wide(controller)?;
    let working_directory = wide(
        controller
            .parent()
            .expect("controller executable has a parent"),
    )?;
    let mut command_line = format!(
        "\"{}\" --manager-session-id {manager_session_id}",
        controller.display()
    )
    .encode_utf16()
    .chain([0])
    .collect::<Vec<_>>();
    let mut environment = std::ptr::null_mut();
    unsafe {
        CreateEnvironmentBlock(&mut environment, Some(token.0), false)?;
    }
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).expect("startup structure size fits u32"),
        ..Default::default()
    };
    let mut information = PROCESS_INFORMATION::default();
    let result = unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(environment),
            PCWSTR(working_directory.as_ptr()),
            &startup,
            &mut information,
        )
    };
    unsafe {
        let _ = DestroyEnvironmentBlock(environment);
    }
    result?;
    unsafe {
        let _ = CloseHandle(information.hThread);
    }
    let process = OwnedHandle(information.hProcess);
    let creation_time = process_creation_time(&process)?;
    Ok((
        process,
        ProcessIdentity {
            process_id: information.dwProcessId,
            creation_time,
        },
    ))
}

fn open_existing(
    expected: ProcessIdentity,
    expected_session_id: u32,
) -> Result<(OwnedHandle, ProcessIdentity), LaunchError> {
    let process = OwnedHandle(unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            false,
            expected.process_id,
        )?
    });
    if process_creation_time(&process)? != expected.creation_time {
        return Err(LaunchError::InvalidInstallation(
            "recorded controller PID was reused".to_owned(),
        ));
    }
    let mut session_id = 0;
    unsafe {
        ProcessIdToSessionId(expected.process_id, &mut session_id)?;
    }
    if session_id != expected_session_id {
        return Err(LaunchError::InvalidInstallation(
            "recorded controller belongs to another Windows session".to_owned(),
        ));
    }
    Ok((process, expected))
}

fn process_creation_time(process: &OwnedHandle) -> windows::core::Result<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(process.0, &mut creation, &mut exit, &mut kernel, &mut user)?;
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn user_local_app_data(token: &OwnedHandle) -> Result<PathBuf, LaunchError> {
    let mut environment = std::ptr::null_mut();
    unsafe {
        CreateEnvironmentBlock(&mut environment, Some(token.0), false)?;
    }
    let mut cursor = environment.cast::<u16>();
    let mut local_app_data = None;
    unsafe {
        loop {
            if *cursor == 0 {
                break;
            }
            let mut end = cursor;
            while *end != 0 {
                end = end.add(1);
            }
            let length = end.offset_from(cursor) as usize;
            let entry = String::from_utf16_lossy(std::slice::from_raw_parts(cursor, length));
            if let Some((name, value)) = entry.split_once('=')
                && name.eq_ignore_ascii_case("LOCALAPPDATA")
            {
                local_app_data = Some(PathBuf::from(value));
                break;
            }
            cursor = end.add(1);
        }
        let _ = DestroyEnvironmentBlock(environment);
    }
    local_app_data.ok_or(LaunchError::LocalAppDataMissing)
}

fn wide(path: &Path) -> windows::core::Result<Vec<u16>> {
    let value = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    if value[..value.len() - 1].contains(&0) {
        Err(windows::core::Error::new(
            windows::Win32::Foundation::E_INVALIDARG,
            "path contains a null character",
        ))
    } else {
        Ok(value)
    }
}

use std::os::windows::ffi::OsStrExt;

struct OwnedHandle(HANDLE);

unsafe impl Send for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
