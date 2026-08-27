#![cfg(windows)]

mod installer;

use std::{error::Error, io::Write, path::PathBuf, time::Duration};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use serde_json::json;
use susm_protocol::{
    MAX_MESSAGE_SIZE,
    control::{
        CancelRequest, DisableRequest, EnableRequest, GetExecutionRequest, GetWorkloadRequest,
        ListExecutionsRequest, ListWorkloadsRequest, ReadLogsRequest, ReloadRequest, RerunRequest,
        RestartRequest, RunRequest, StartRequest, StopRequest, Workload,
        control_service_client::ControlServiceClient,
    },
    host::{
        GetControllerStatusRequest, RegisterUserRequest, RestartControllerRequest,
        UnregisterUserRequest, host_control_service_client::HostControlServiceClient,
    },
    pipe::{connect_for, control_pipe_name, current_user_sid, host_pipe_name},
};

#[derive(Debug, Parser)]
#[command(name = "susm", version, about = "Sucking User Service Manager")]
struct Arguments {
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Reload,
    List,
    Status {
        workload: String,
    },
    Start {
        service: String,
    },
    Stop {
        service: String,
    },
    Restart {
        service: String,
    },
    Run {
        job: String,
    },
    Cancel {
        job: String,
    },
    Rerun {
        job: String,
    },
    Enable {
        workload: String,
    },
    Disable {
        workload: String,
    },
    Executions {
        workload: String,
    },
    Execution {
        execution_id: String,
    },
    Logs {
        workload: String,
        #[arg(long)]
        execution: Option<String>,
        #[arg(long)]
        attempt: Option<u32>,
        #[arg(long, value_enum, default_value_t = LogStream::All)]
        stream: LogStream,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        raw: bool,
    },
    Controller {
        #[command(subcommand)]
        command: ControllerCommand,
    },
    Install {
        #[arg(long)]
        user: bool,
        source: Option<PathBuf>,
    },
    Uninstall {
        #[arg(long)]
        user: bool,
    },
    Upgrade {
        source: Option<PathBuf>,
        #[command(subcommand)]
        command: Option<UpgradeCommand>,
    },
    Rollback {
        version_or_manifest_prefix: String,
    },
    Versions,
    Completions {
        shell: Shell,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogStream {
    Stdout,
    Stderr,
    All,
}

impl LogStream {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Path,
}

#[derive(Debug, Subcommand)]
enum ControllerCommand {
    Status,
    Restart,
}

#[derive(Debug, Subcommand)]
enum UpgradeCommand {
    Gc,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("susm: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let arguments = Arguments::parse();
    match arguments.command {
        Command::Config {
            command: ConfigCommand::Path,
        } => println!("{}", config_directory()?.display()),
        Command::Completions { shell } => {
            generate(
                shell,
                &mut Arguments::command(),
                "susm",
                &mut std::io::stdout(),
            );
        }
        Command::Install { user, source } => {
            if !user {
                return Err("only per-user installation is supported; pass --user".into());
            }
            let source = match source {
                Some(source) => source,
                None => std::env::current_exe()?
                    .parent()
                    .ok_or("current executable has no parent directory")?
                    .to_owned(),
            };
            let installed = installer::install(&source)?;
            std::fs::create_dir_all(config_directory()?)?;
            let status = host_register().await?;
            println!(
                "installed {} ({}) at {}; controller pid {}",
                installed.version,
                installed.identity,
                installed.path.display(),
                status.controller_process_id
            );
        }
        Command::Uninstall { user } => {
            if !user {
                return Err("only per-user uninstallation is supported; pass --user".into());
            }
            let _ = host_unregister().await?;
            installer::uninstall_user()?;
            println!("per-user SUSM registration and stable binaries removed");
        }
        Command::Upgrade { source, command } => match (source, command) {
            (Some(source), None) => {
                let installed = installer::install(&source)?;
                let _ = host_restart().await?;
                println!(
                    "selected {} ({}) at {}",
                    installed.version,
                    installed.identity,
                    installed.path.display()
                );
            }
            (None, Some(UpgradeCommand::Gc)) => {
                let removed = installer::garbage_collect()?;
                println!("removed {removed} unused version or staging entries");
            }
            _ => return Err("provide one bundle path or the gc subcommand".into()),
        },
        Command::Rollback {
            version_or_manifest_prefix,
        } => {
            let installed = installer::rollback(&version_or_manifest_prefix)?;
            let _ = host_restart().await?;
            println!(
                "selected {} ({}) at {}",
                installed.version,
                installed.identity,
                installed.path.display()
            );
        }
        Command::Versions => {
            let versions = installer::list_versions()?;
            if arguments.json {
                println!(
                    "{}",
                    serde_json::Value::Array(
                        versions
                            .iter()
                            .map(|version| json!({
                                "version": version.version,
                                "identity": version.identity,
                                "path": version.path,
                                "current": version.current,
                                "pin_count": version.pin_count,
                            }))
                            .collect()
                    )
                );
            } else {
                for version in versions {
                    println!(
                        "{}\t{}\t{}\t{} pins\t{}",
                        version.version,
                        version.identity,
                        if version.current {
                            "current"
                        } else {
                            "installed"
                        },
                        version.pin_count,
                        version.path.display()
                    );
                }
            }
        }
        Command::Controller {
            command: ControllerCommand::Status,
        } => {
            let status = host_status().await?;
            if arguments.json {
                println!(
                    "{}",
                    json!({
                        "registered": status.registered,
                        "running": status.controller_running,
                        "manager_session_id": status.manager_session_id,
                        "process_id": status.controller_process_id,
                        "message": status.message,
                    })
                );
            } else if status.controller_running {
                println!(
                    "controller is running (pid {})",
                    status.controller_process_id
                );
            } else if status.registered {
                if status.message.is_empty() {
                    println!("controller is registered and recovering");
                } else {
                    println!("controller is registered: {}", status.message);
                }
            } else {
                println!("user is not registered");
            }
        }
        Command::Controller {
            command: ControllerCommand::Restart,
        } => {
            let status = host_restart().await?;
            println!(
                "controller restart requested for manager session {}",
                status.manager_session_id
            );
        }
        command => run_rpc(command, arguments.json).await?,
    }
    Ok(())
}

async fn run_rpc(command: Command, json_output: bool) -> Result<(), Box<dyn Error>> {
    let sid = current_user_sid()?;
    let status = host_status().await?;
    if !status.controller_running || status.manager_session_id.is_empty() {
        return Err("no controller is running for the current manager session".into());
    }
    uuid::Uuid::parse_str(&status.manager_session_id)?;
    let channel = connect_for(
        control_pipe_name(&sid, &status.manager_session_id),
        Duration::from_secs(5),
    )
    .await?;
    let mut client = ControlServiceClient::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_SIZE)
        .max_encoding_message_size(MAX_MESSAGE_SIZE);
    match command {
        Command::Reload => {
            let response = client.reload(ReloadRequest {}).await?.into_inner();
            if json_output {
                println!(
                    "{}",
                    json!({
                        "changed": response.changed,
                        "generation": response.generation,
                        "diagnostics": response.diagnostics.into_iter().map(|item| json!({
                            "path": item.path,
                            "message": item.message,
                        })).collect::<Vec<_>>()
                    })
                );
            } else if response.diagnostics.is_empty() {
                println!(
                    "configuration {} ({})",
                    if response.changed {
                        "reloaded"
                    } else {
                        "unchanged"
                    },
                    response.generation
                );
            } else {
                for diagnostic in response.diagnostics {
                    eprintln!("{}: {}", diagnostic.path, diagnostic.message);
                }
                return Err("configuration was not reloaded".into());
            }
        }
        Command::List => {
            let workloads = client
                .list_workloads(ListWorkloadsRequest {
                    page_size: 1000,
                    cursor: String::new(),
                })
                .await?
                .into_inner()
                .workloads;
            print_workloads(&workloads, json_output);
        }
        Command::Status { workload } => {
            let workload = client
                .get_workload(GetWorkloadRequest {
                    workload_id: workload,
                })
                .await?
                .into_inner()
                .workload
                .ok_or("controller returned no workload")?;
            print_workloads(&[workload], json_output);
        }
        Command::Start { service } => {
            let response = client
                .start(StartRequest {
                    workload_id: service,
                })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output)?;
        }
        Command::Stop { service } => {
            let response = client
                .stop(StopRequest {
                    workload_id: service,
                })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output)?;
        }
        Command::Restart { service } => {
            let response = client
                .restart(RestartRequest {
                    workload_id: service,
                })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output)?;
        }
        Command::Run { job } => {
            let response = client
                .run(RunRequest { workload_id: job })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output)?;
        }
        Command::Cancel { job } => {
            let response = client
                .cancel(CancelRequest { workload_id: job })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output)?;
        }
        Command::Rerun { job } => {
            let response = client
                .rerun(RerunRequest { workload_id: job })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output)?;
        }
        Command::Enable { workload } => {
            let response = client
                .enable(EnableRequest {
                    workload_id: workload,
                })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output)?;
        }
        Command::Disable { workload } => {
            let response = client
                .disable(DisableRequest {
                    workload_id: workload,
                })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output)?;
        }
        Command::Executions { workload } => {
            let executions = client
                .list_executions(ListExecutionsRequest {
                    workload_id: workload,
                    page_size: 100,
                    cursor: String::new(),
                })
                .await?
                .into_inner()
                .executions;
            if json_output {
                println!(
                    "{}",
                    serde_json::Value::Array(executions.iter().map(execution_json).collect())
                );
            } else {
                for execution in executions {
                    println!(
                        "{}\t{}\tsupervisor {}\tworkload {}\tattempt {}\t{}",
                        execution.execution_id,
                        execution.state,
                        execution.supervisor_process_id,
                        execution.workload_process_id,
                        execution.attempt,
                        execution
                            .exit_code
                            .map_or_else(|| "-".to_owned(), |value| value.to_string())
                    );
                }
            }
        }
        Command::Execution { execution_id } => {
            let execution = client
                .get_execution(GetExecutionRequest { execution_id })
                .await?
                .into_inner()
                .execution
                .ok_or("controller returned no execution")?;
            if json_output {
                println!("{}", execution_json(&execution));
            } else {
                println!("execution: {}", execution.execution_id);
                println!("workload:  {}", execution.workload_id);
                println!("state:     {}", execution.state);
                println!("supervisor process: {}", execution.supervisor_process_id);
                println!("workload process:   {}", execution.workload_process_id);
                println!("attempt:            {}", execution.attempt);
                if let Some(exit_code) = execution.exit_code {
                    println!("exit code: {exit_code}");
                }
                if !execution.error.is_empty() {
                    println!("error:     {}", execution.error);
                }
            }
        }
        Command::Logs {
            workload,
            execution,
            attempt,
            stream,
            follow,
            raw,
        } => {
            if raw && matches!(stream, LogStream::All) {
                return Err("--raw requires --stream stdout or --stream stderr".into());
            }
            let mut records = client
                .read_logs(ReadLogsRequest {
                    workload_id: workload,
                    execution_id: execution.unwrap_or_default(),
                    attempt: attempt.unwrap_or(0),
                    stream: stream.as_str().to_owned(),
                    follow,
                })
                .await?
                .into_inner();
            while let Some(record) = records.message().await? {
                if raw {
                    std::io::stdout().write_all(&record.message)?;
                } else if json_output {
                    println!(
                        "{}",
                        json!({
                            "execution_id": record.execution_id,
                            "attempt": record.attempt,
                            "stream": record.stream,
                            "sequence": record.sequence,
                            "timestamp_unix_ms": record.timestamp_unix_ms,
                            "message": String::from_utf8_lossy(&record.message),
                            "gap": record.gap,
                        })
                    );
                } else {
                    print!(
                        "{} {} {}: {}",
                        record.timestamp_unix_ms,
                        record.attempt,
                        record.stream,
                        String::from_utf8_lossy(&record.message)
                    );
                }
            }
        }
        Command::Config { .. }
        | Command::Completions { .. }
        | Command::Install { .. }
        | Command::Uninstall { .. }
        | Command::Upgrade { .. }
        | Command::Rollback { .. }
        | Command::Versions
        | Command::Controller { .. } => unreachable!("handled before control RPC"),
    }
    Ok(())
}

fn print_mutation(
    changed: bool,
    workload: Option<Workload>,
    json_output: bool,
) -> Result<(), Box<dyn Error>> {
    let workload = workload.ok_or("controller returned no workload")?;
    if json_output {
        println!(
            "{}",
            json!({ "changed": changed, "workload": workload_json(&workload) })
        );
    } else {
        println!(
            "{}: {}{}",
            workload.workload_id,
            workload.state,
            if changed { "" } else { " (unchanged)" }
        );
    }
    Ok(())
}

fn print_workloads(workloads: &[Workload], json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::Value::Array(workloads.iter().map(workload_json).collect())
        );
    } else if workloads.is_empty() {
        println!("no workloads");
    } else {
        for workload in workloads {
            println!(
                "{}\t{}\t{}\t{}{}{}{}{}",
                workload.workload_id,
                workload.kind,
                workload.state,
                if workload.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                if workload.supervisor_process_id == 0 {
                    String::new()
                } else {
                    format!("\tsupervisor {}", workload.supervisor_process_id)
                },
                if workload.workload_process_id == 0 {
                    String::new()
                } else {
                    format!("\tworkload {}", workload.workload_process_id)
                },
                if workload.attempt == 0 {
                    String::new()
                } else {
                    format!("\tattempt {}", workload.attempt)
                },
                if workload.error.is_empty() {
                    String::new()
                } else {
                    format!("\terror {}", workload.error)
                }
            );
        }
    }
}

fn workload_json(workload: &Workload) -> serde_json::Value {
    json!({
        "workload_id": workload.workload_id,
        "kind": workload.kind,
        "enabled": workload.enabled,
        "state": workload.state,
        "execution_id": workload.execution_id,
        "supervisor_process_id": workload.supervisor_process_id,
        "workload_process_id": workload.workload_process_id,
        "attempt": workload.attempt,
        "error": workload.error,
        "last_outcome": workload.last_outcome,
        "definition_missing": workload.definition_missing,
        "restart_required": workload.restart_required,
        "policy_sync_pending": workload.policy_sync_pending,
    })
}

fn execution_json(execution: &susm_protocol::control::Execution) -> serde_json::Value {
    json!({
        "execution_id": execution.execution_id,
        "workload_id": execution.workload_id,
        "state": execution.state,
        "supervisor_process_id": execution.supervisor_process_id,
        "workload_process_id": execution.workload_process_id,
        "attempt": execution.attempt,
        "started_unix_ms": execution.started_unix_ms,
        "ended_unix_ms": execution.ended_unix_ms,
        "exit_code": execution.exit_code,
        "error": execution.error,
    })
}

fn config_directory() -> Result<PathBuf, Box<dyn Error>> {
    let profile = std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .ok_or("USERPROFILE is missing")?;
    Ok(PathBuf::from(profile)
        .join(".config")
        .join("susm")
        .join("workloads.d"))
}

async fn host_client() -> Result<HostControlServiceClient<tonic::transport::Channel>, Box<dyn Error>>
{
    let channel = connect_for(host_pipe_name(), Duration::from_secs(5)).await?;
    Ok(HostControlServiceClient::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_SIZE)
        .max_encoding_message_size(MAX_MESSAGE_SIZE))
}

async fn host_register() -> Result<susm_protocol::host::HostStatus, Box<dyn Error>> {
    host_client()
        .await?
        .register_user(RegisterUserRequest {})
        .await?
        .into_inner()
        .status
        .ok_or_else(|| "host returned no status".into())
}

async fn host_unregister() -> Result<susm_protocol::host::HostStatus, Box<dyn Error>> {
    host_client()
        .await?
        .unregister_user(UnregisterUserRequest {})
        .await?
        .into_inner()
        .status
        .ok_or_else(|| "host returned no status".into())
}

async fn host_status() -> Result<susm_protocol::host::HostStatus, Box<dyn Error>> {
    host_client()
        .await?
        .get_controller_status(GetControllerStatusRequest {})
        .await?
        .into_inner()
        .status
        .ok_or_else(|| "host returned no status".into())
}

async fn host_restart() -> Result<susm_protocol::host::HostStatus, Box<dyn Error>> {
    host_client()
        .await?
        .restart_controller(RestartControllerRequest {})
        .await?
        .into_inner()
        .status
        .ok_or_else(|| "host returned no status".into())
}
