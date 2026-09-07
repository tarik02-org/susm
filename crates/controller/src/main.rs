#![cfg(windows)]

use std::{error::Error, future::pending, path::PathBuf, time::Duration};

use clap::Parser;
use susm_controller::{ControlRpc, Manager, ManagerPaths, SupervisorRpc};
use susm_diagnostics::{Component, init as init_diagnostics};
use susm_protocol::{
    MAX_MESSAGE_SIZE,
    control::control_service_server::ControlServiceServer,
    pipe::{PipeIncoming, control_pipe_name, current_user_sid, supervisor_pipe_name},
    session::EndingEvent,
    supervisor::supervisor_control_service_server::SupervisorControlServiceServer,
};
use tonic::transport::Server;

#[derive(Debug, Parser)]
#[command(name = "susmd", version, about = "SUSM per-user controller")]
struct Arguments {
    #[arg(long)]
    config_directory: Option<PathBuf>,
    #[arg(long)]
    data_directory: Option<PathBuf>,
    #[arg(long)]
    supervisor: Option<PathBuf>,
    #[arg(long)]
    manager_session_id: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments = Arguments::parse();
    let profile = required_environment("USERPROFILE")?;
    let local_app_data = required_environment("LOCALAPPDATA")?;
    let _diagnostics = init_diagnostics(
        Component::Controller,
        &local_app_data
            .join("susm")
            .join("diagnostics")
            .join("controller"),
    )?;
    let executable_directory = std::env::current_exe()?
        .parent()
        .ok_or("controller executable has no parent directory")?
        .to_owned();
    if let Some(manager_session_id) = &arguments.manager_session_id {
        uuid::Uuid::parse_str(manager_session_id)?;
    }
    let ending_event = arguments
        .manager_session_id
        .as_deref()
        .map(EndingEvent::open)
        .transpose()?;
    let manager_session_id = arguments.manager_session_id.clone().unwrap_or_default();
    tracing::info!(
        name = "controller_starting",
        manager_session_id = %manager_session_id,
    );
    let paths = ManagerPaths {
        config_directory: arguments
            .config_directory
            .unwrap_or_else(|| profile.join(".config").join("susm").join("workloads.d")),
        data_directory: arguments
            .data_directory
            .unwrap_or_else(|| local_app_data.join("susm")),
        supervisor_executable: arguments
            .supervisor
            .unwrap_or_else(|| executable_directory.join("susm-supervisor.exe")),
        manager_session_id: arguments.manager_session_id,
    };
    let manager = Manager::open(paths)?;
    if manager.config_directory().is_dir()
        && let Err(error) = manager.reload()
    {
        tracing::warn!(
            name = "initial_configuration_reload_failed",
            manager_session_id = %manager_session_id,
            error = %error,
        );
    }
    manager.start_recovery_monitors();
    manager.start_enabled();

    let sid = current_user_sid()?;
    let control_incoming =
        PipeIncoming::bind(control_pipe_name(&sid, &manager_session_id), sid.clone());
    let supervisor_incoming =
        PipeIncoming::bind(supervisor_pipe_name(&sid, &manager_session_id), sid);
    let control_service = ControlServiceServer::new(ControlRpc::new(manager.clone()))
        .max_decoding_message_size(MAX_MESSAGE_SIZE)
        .max_encoding_message_size(MAX_MESSAGE_SIZE);
    let supervisor_service =
        SupervisorControlServiceServer::new(SupervisorRpc::new(manager.clone()))
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let supervisor_shutdown = shutdown_rx.clone();
    let control = tokio::spawn(
        Server::builder()
            .add_service(control_service)
            .serve_with_incoming_shutdown(control_incoming, wait_for_shutdown(shutdown_rx)),
    );
    let supervisor = tokio::spawn(
        Server::builder()
            .add_service(supervisor_service)
            .serve_with_incoming_shutdown(
                supervisor_incoming,
                wait_for_shutdown(supervisor_shutdown),
            ),
    );
    let session_ending = tokio::select! {
        _ = shutdown_signal() => false,
        result = wait_for_manager_session(ending_event.as_ref()) => {
            result?;
            true
        }
    };
    if session_ending {
        tracing::info!(
            name = "manager_session_ending",
            manager_session_id = %manager_session_id,
        );
        manager.end_session();
        let _ = tokio::time::timeout(Duration::from_secs(30), async {
            while !manager.is_quiescent() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
    }
    let _ = shutdown.send(true);
    control.await??;
    supervisor.await??;
    tracing::info!(
        name = "controller_stopped",
        manager_session_id = %manager_session_id,
    );
    Ok(())
}

fn required_environment(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("required environment variable {name} is missing").into())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn wait_for_manager_session(
    event: Option<&EndingEvent>,
) -> Result<(), susm_protocol::session::SessionEventError> {
    match event {
        Some(event) => event.wait().await,
        None => pending().await,
    }
}

async fn wait_for_shutdown(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}
