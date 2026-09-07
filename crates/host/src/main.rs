#![cfg(windows)]

mod launcher;
mod registration;
mod rpc;

use std::{
    error::Error,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read},
    mem::size_of,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use clap::{Parser, Subcommand};
use rpc::HostRpc;
use sha2::{Digest, Sha256};
use susm_diagnostics::{Component, init as init_diagnostics};
use susm_protocol::{
    MAX_MESSAGE_SIZE,
    host::host_control_service_server::HostControlServiceServer,
    pipe::{PipeIncoming, host_pipe_name},
};
use tonic::transport::Server;
use windows::{
    Win32::{
        Foundation::ERROR_SERVICE_DOES_NOT_EXIST,
        System::{
            RemoteDesktop::WTSSESSION_NOTIFICATION,
            Services::{
                ChangeServiceConfig2W, SC_HANDLE, SERVICE_ACCEPT_PRESHUTDOWN,
                SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
                SERVICE_CONTROL_PRESHUTDOWN, SERVICE_CONTROL_SESSIONCHANGE,
                SERVICE_REQUIRED_PRIVILEGES_INFOW,
            },
        },
        UI::WindowsAndMessaging::{WTS_SESSION_LOGOFF, WTS_SESSION_LOGON},
    },
    core::PWSTR,
};
use windows_service::{
    Error as WindowsServiceError,
    service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
        ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo,
        ServiceStartType as WindowsServiceStartType, ServiceState as WindowsServiceState,
        ServiceType,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use windows_services::{Command as ServiceCommand, Service, State as ServiceState};

const SERVICE_NAME: &str = "SUSMHost";

#[derive(Debug, Parser)]
#[command(name = "susm-host", version, about = "SUSM machine service host")]
struct Arguments {
    #[command(subcommand)]
    command: HostCommand,
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    Install,
    Uninstall,
    Service,
    Console,
}

fn main() -> Result<(), Box<dyn Error>> {
    match Arguments::parse().command {
        HostCommand::Install => install(),
        HostCommand::Uninstall => uninstall(),
        HostCommand::Service => run_service(),
        HostCommand::Console => {
            let (_commands, receiver) = tokio::sync::mpsc::unbounded_channel();
            run_host(receiver)
        }
    }
}

fn install() -> Result<(), Box<dyn Error>> {
    let program_files = std::env::var_os("ProgramFiles").ok_or("ProgramFiles is missing")?;
    let root = PathBuf::from(program_files).join("SUSM").join("host");
    let current = std::env::current_exe()?;
    let digest = digest_file(&current)?;
    let identity = format!("{}-{digest}", env!("CARGO_PKG_VERSION"));
    let directory = root.join("versions").join(&identity);
    let installed = install_host_image(&current, &root, &directory, &digest)?;
    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let manager = ServiceManager::local_computer(None::<&str>, manager_access)?;
    let service_access = ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::STOP;
    let service_info = service_info(installed);
    let (service, exists) = match manager.open_service(SERVICE_NAME, service_access) {
        Ok(service) => {
            service.change_config(&service_info)?;
            (service, true)
        }
        Err(error) if service_does_not_exist(&error) => (
            manager.create_service(&service_info, service_access)?,
            false,
        ),
        Err(error) => return Err(error.into()),
    };
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            restart_action(Duration::from_secs(1)),
            restart_action(Duration::from_secs(5)),
            restart_action(Duration::from_secs(30)),
        ]),
    })?;
    service.set_failure_actions_on_non_crash_failures(true)?;
    service.set_preshutdown_timeout(Duration::from_secs(45))?;
    set_required_privileges(&service)?;
    service.set_description("Starts and recovers registered SUSM per-user controllers")?;
    if exists {
        let _ = service.stop();
        wait_for_service_stopped(&service)?;
    }
    service.start::<&OsStr>(&[])?;
    Ok(())
}

fn uninstall() -> Result<(), Box<dyn Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    let _ = service.stop();
    service.delete()?;
    Ok(())
}

fn service_info(executable_path: PathBuf) -> ServiceInfo {
    ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("SUSM Host"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: WindowsServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path,
        launch_arguments: vec![OsString::from("service")],
        dependencies: Vec::new(),
        account_name: Some(OsString::from("LocalSystem")),
        account_password: None,
    }
}

fn restart_action(delay: Duration) -> ServiceAction {
    ServiceAction {
        action_type: ServiceActionType::Restart,
        delay,
    }
}

fn service_does_not_exist(error: &WindowsServiceError) -> bool {
    matches!(
        error,
        WindowsServiceError::Winapi(error)
            if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST.0 as i32)
    )
}

fn set_required_privileges(
    service: &windows_service::service::Service,
) -> windows::core::Result<()> {
    let mut privileges =
        "SeTcbPrivilege\0SeAssignPrimaryTokenPrivilege\0SeIncreaseQuotaPrivilege\0\0"
            .encode_utf16()
            .collect::<Vec<_>>();
    let info = SERVICE_REQUIRED_PRIVILEGES_INFOW {
        pmszRequiredPrivileges: PWSTR(privileges.as_mut_ptr()),
    };
    unsafe {
        ChangeServiceConfig2W(
            SC_HANDLE(service.raw_handle()),
            SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
            Some((&raw const info).cast()),
        )
    }
}

fn install_host_image(
    source: &Path,
    root: &Path,
    directory: &Path,
    expected_digest: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let installed = directory.join("susm-host.exe");
    if directory.exists() {
        if installed.is_file() && digest_file(&installed)? == expected_digest {
            return Ok(installed);
        }
        return Err(format!("host installation is corrupt: {}", directory.display()).into());
    }
    fs::create_dir_all(root.join("versions"))?;
    let staging = root.join(format!(
        ".{}.{}.tmp",
        env!("CARGO_PKG_VERSION"),
        uuid::Uuid::now_v7()
    ));
    fs::create_dir(&staging)?;
    let staged = staging.join("susm-host.exe");
    let result = (|| -> Result<(), Box<dyn Error>> {
        fs::copy(source, &staged)?;
        fs::OpenOptions::new()
            .write(true)
            .open(&staged)?
            .sync_all()?;
        if digest_file(&staged)? != expected_digest {
            return Err("copied host image failed digest verification".into());
        }
        fs::rename(&staging, directory)?;
        Ok(())
    })();
    if result.is_err() && staging.starts_with(root) {
        let _ = fs::remove_dir_all(&staging);
    }
    result?;
    Ok(installed)
}

fn digest_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let mut value = String::with_capacity(64);
    use std::fmt::Write as _;
    for byte in digest.finalize() {
        write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(value)
}

fn wait_for_service_stopped(
    service: &windows_service::service::Service,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..140 {
        if service.query_status()?.current_state == WindowsServiceState::Stopped {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err("SUSMHost did not stop within 35 seconds".into())
}

fn run_service() -> Result<(), Box<dyn Error>> {
    let worker = Arc::new(Mutex::new(None::<Worker>));
    let callback_worker = worker.clone();
    let mut service = Service::new();
    service
        .can_stop()
        .can_accept(SERVICE_ACCEPT_SESSIONCHANGE | SERVICE_ACCEPT_PRESHUTDOWN)
        .run(move |service, command| match command {
            ServiceCommand::Start => {
                let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
                let thread = thread::spawn(move || {
                    let _ = run_host(receiver);
                });
                *callback_worker
                    .lock()
                    .expect("service worker lock poisoned") = Some(Worker { commands, thread });
            }
            ServiceCommand::Stop => {
                if let Some(worker) = callback_worker
                    .lock()
                    .expect("service worker lock poisoned")
                    .take()
                {
                    worker.finish(HostRuntimeCommand::Detach);
                }
            }
            ServiceCommand::Extended(command)
                if command.control == SERVICE_CONTROL_SESSIONCHANGE =>
            {
                let Some(session_id) = session_notification(command.data) else {
                    return;
                };
                let runtime_command = match command.ty {
                    WTS_SESSION_LOGON => HostRuntimeCommand::Reconcile,
                    WTS_SESSION_LOGOFF => HostRuntimeCommand::EndSession(session_id),
                    _ => return,
                };
                if let Some(worker) = callback_worker
                    .lock()
                    .expect("service worker lock poisoned")
                    .as_ref()
                {
                    let _ = worker.commands.send(runtime_command);
                }
            }
            ServiceCommand::Extended(command) if command.control == SERVICE_CONTROL_PRESHUTDOWN => {
                service.set_state(ServiceState::StopPending);
                if let Some(worker) = callback_worker
                    .lock()
                    .expect("service worker lock poisoned")
                    .take()
                {
                    worker.finish(HostRuntimeCommand::Preshutdown);
                }
                service.set_state(ServiceState::Stopped);
            }
            _ => {}
        })
        .map_err(Into::into)
}

struct Worker {
    commands: tokio::sync::mpsc::UnboundedSender<HostRuntimeCommand>,
    thread: thread::JoinHandle<()>,
}

impl Worker {
    fn finish(self, command: HostRuntimeCommand) {
        let _ = self.commands.send(command);
        let _ = self.thread.join();
    }
}

#[derive(Clone, Copy)]
enum HostRuntimeCommand {
    Reconcile,
    EndSession(u32),
    Detach,
    Preshutdown,
}

fn session_notification(data: *const core::ffi::c_void) -> Option<u32> {
    if data.is_null() {
        return None;
    }
    let notification = unsafe { &*data.cast::<WTSSESSION_NOTIFICATION>() };
    (notification.cbSize as usize == size_of::<WTSSESSION_NOTIFICATION>())
        .then_some(notification.dwSessionId)
}

fn run_host(
    receiver: tokio::sync::mpsc::UnboundedReceiver<HostRuntimeCommand>,
) -> Result<(), Box<dyn Error>> {
    let program_data = std::env::var_os("ProgramData")
        .filter(|value| !value.is_empty())
        .ok_or("ProgramData is missing")?;
    let _diagnostics = init_diagnostics(
        Component::Host,
        &PathBuf::from(program_data)
            .join("SUSM")
            .join("diagnostics")
            .join("host"),
    )?;
    tracing::info!(name = "host_starting");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let rpc = Arc::new(HostRpc::new()?);
        let incoming = PipeIncoming::bind_authenticated_users(host_pipe_name());
        let service = HostControlServiceServer::from_arc(rpc.clone())
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE);
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(
            Server::builder()
                .add_service(service)
                .serve_with_incoming_shutdown(incoming, async move {
                    while !*shutdown_rx.borrow() {
                        if shutdown_rx.changed().await.is_err() {
                            return;
                        }
                    }
                }),
        );
        run_host_commands(rpc, receiver).await;
        let _ = shutdown.send(true);
        server.await??;
        Ok::<(), Box<dyn Error>>(())
    })?;
    tracing::info!(name = "host_stopped");
    Ok(())
}

async fn run_host_commands(
    rpc: Arc<HostRpc>,
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<HostRuntimeCommand>,
) {
    let mut reconciliation = tokio::time::interval(std::time::Duration::from_secs(5));
    reconciliation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = reconciliation.tick() => rpc.reconcile(),
            command = receiver.recv() => match command {
                Some(HostRuntimeCommand::Reconcile) => rpc.reconcile(),
                Some(HostRuntimeCommand::EndSession(session_id)) => {
                    rpc.end_windows_session(session_id);
                }
                Some(HostRuntimeCommand::Preshutdown) => {
                    rpc.end_all();
                    return;
                }
                Some(HostRuntimeCommand::Detach) | None => {
                    rpc.detach_all();
                    return;
                }
            }
        }
    }
}
