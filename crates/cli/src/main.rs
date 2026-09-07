#![cfg(windows)]

mod installer;
mod output;

use std::{error::Error, path::PathBuf, time::Duration};

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

use output::{ColorMode, HumanOutput, LogOptions};

#[derive(Debug, Parser)]
#[command(
    name = "susm",
    version,
    about = "Sucking User Service Manager",
    arg_required_else_help = true
)]
struct Arguments {
    /// Emit stable JSON instead of human-readable output.
    #[arg(long, global = true)]
    json: bool,
    /// Control colors in SUSM output. Workload log bytes are never changed.
    #[arg(long, global = true, value_enum, default_value_t)]
    color: ColorMode,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect configuration paths.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Atomically reload workload definitions.
    Reload,
    /// List managed workloads.
    List,
    /// Show one workload.
    Status {
        /// Workload name.
        workload: String,
    },
    /// Start a service.
    Start {
        /// Service name.
        service: String,
    },
    /// Stop a service.
    Stop {
        /// Service name.
        service: String,
    },
    /// Restart a service with a new execution.
    Restart {
        /// Service name.
        service: String,
    },
    /// Run a job.
    Run {
        /// Job name.
        job: String,
    },
    /// Cancel a running job.
    Cancel {
        /// Job name.
        job: String,
    },
    /// Run a job again with a new execution.
    Rerun {
        /// Job name.
        job: String,
    },
    /// Start a workload automatically in each manager session.
    Enable {
        /// Workload name.
        workload: String,
    },
    /// Disable automatic start for a workload.
    Disable {
        /// Workload name.
        workload: String,
    },
    /// List a workload's execution history.
    Executions {
        /// Workload name.
        workload: String,
    },
    /// Show one execution.
    Execution {
        /// Execution UUID.
        execution_id: String,
    },
    /// Print or follow workload output.
    Logs {
        /// Workload name.
        workload: String,
        /// Select an execution instead of the active or newest execution.
        #[arg(long)]
        execution: Option<String>,
        /// Select one attempt. Zero means all attempts.
        #[arg(long)]
        attempt: Option<u32>,
        /// Select stdout, stderr, or both.
        #[arg(long, value_enum, default_value_t = LogStream::All)]
        stream: LogStream,
        /// Continue printing new output.
        #[arg(short, long)]
        follow: bool,
        /// Prefix each displayed line with its UTC timestamp.
        #[arg(short = 't', long, conflicts_with = "json")]
        timestamps: bool,
        /// Prefix each displayed line with its stream and attempt.
        #[arg(long, conflicts_with = "json")]
        prefix: bool,
    },
    /// Inspect or restart the per-user controller.
    Controller {
        #[command(subcommand)]
        command: ControllerCommand,
    },
    /// Install and register a per-user bundle.
    Install {
        /// Confirm this is a per-user installation.
        #[arg(long)]
        user: bool,
        /// Bundle directory or ZIP. Defaults to the directory beside susm.exe.
        source: Option<PathBuf>,
    },
    /// Unregister and remove stable per-user binaries.
    Uninstall {
        /// Confirm this is a per-user uninstallation.
        #[arg(long)]
        user: bool,
    },
    /// Install and select another user bundle.
    Upgrade {
        /// Bundle directory or ZIP.
        source: Option<PathBuf>,
        #[command(subcommand)]
        command: Option<UpgradeCommand>,
    },
    /// Select a previously installed version.
    Rollback {
        /// Exact version or unambiguous manifest identity prefix.
        version_or_manifest_prefix: String,
    },
    /// List installed user versions.
    Versions,
    /// Generate shell completion code.
    Completions {
        /// Target shell.
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
    /// Print the workload-definition directory.
    Path,
}

#[derive(Debug, Subcommand)]
enum ControllerCommand {
    /// Show registration and controller process state.
    Status,
    /// Ask the host to replace the controller process.
    Restart,
}

#[derive(Debug, Subcommand)]
enum UpgradeCommand {
    /// Remove unused versions and abandoned staging directories.
    Gc,
}

#[tokio::main]
async fn main() {
    let arguments = Arguments::parse();
    let mut output = HumanOutput::new(arguments.color);
    if let Err(error) = run(arguments, &mut output).await {
        let _ = output.error(error.as_ref());
        std::process::exit(1);
    }
}

async fn run(arguments: Arguments, output: &mut HumanOutput) -> Result<(), Box<dyn Error>> {
    let json_output = arguments.json;
    match arguments.command {
        Command::Config {
            command: ConfigCommand::Path,
        } => output.path(&config_directory()?)?,
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
            output.installed(&installed, status.controller_process_id)?;
        }
        Command::Uninstall { user } => {
            if !user {
                return Err("only per-user uninstallation is supported; pass --user".into());
            }
            let _ = host_unregister().await?;
            installer::uninstall_user()?;
            output.uninstalled()?;
        }
        Command::Upgrade { source, command } => match (source, command) {
            (Some(source), None) => {
                let installed = installer::install(&source)?;
                let _ = host_restart().await?;
                output.selected(&installed)?;
            }
            (None, Some(UpgradeCommand::Gc)) => {
                let removed = installer::garbage_collect()?;
                output.garbage_collected(removed)?;
            }
            _ => return Err("provide one bundle path or the gc subcommand".into()),
        },
        Command::Rollback {
            version_or_manifest_prefix,
        } => {
            let installed = installer::rollback(&version_or_manifest_prefix)?;
            let _ = host_restart().await?;
            output.selected(&installed)?;
        }
        Command::Versions => {
            let versions = installer::list_versions()?;
            if json_output {
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
                output.versions(&versions)?;
            }
        }
        Command::Controller {
            command: ControllerCommand::Status,
        } => {
            let status = host_status().await?;
            if json_output {
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
            } else {
                output.controller_status(&status)?;
            }
        }
        Command::Controller {
            command: ControllerCommand::Restart,
        } => {
            let status = host_restart().await?;
            output.controller_restart(&status.manager_session_id)?;
        }
        command => run_rpc(command, json_output, output).await?,
    }
    Ok(())
}

async fn run_rpc(
    command: Command,
    json_output: bool,
    output: &mut HumanOutput,
) -> Result<(), Box<dyn Error>> {
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
                output.reload(response.changed, &response.generation)?;
            } else {
                output.reload_diagnostics(
                    response
                        .diagnostics
                        .iter()
                        .map(|diagnostic| (diagnostic.path.as_str(), diagnostic.message.as_str())),
                )?;
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
            if json_output {
                print_workloads_json(&workloads);
            } else {
                output.workloads(&workloads)?;
            }
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
            if json_output {
                print_workloads_json(&[workload]);
            } else {
                output.workload(&workload)?;
            }
        }
        Command::Start { service } => {
            let response = client
                .start(StartRequest {
                    workload_id: service,
                })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output, output)?;
        }
        Command::Stop { service } => {
            let response = client
                .stop(StopRequest {
                    workload_id: service,
                })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output, output)?;
        }
        Command::Restart { service } => {
            let response = client
                .restart(RestartRequest {
                    workload_id: service,
                })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output, output)?;
        }
        Command::Run { job } => {
            let response = client
                .run(RunRequest { workload_id: job })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output, output)?;
        }
        Command::Cancel { job } => {
            let response = client
                .cancel(CancelRequest { workload_id: job })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output, output)?;
        }
        Command::Rerun { job } => {
            let response = client
                .rerun(RerunRequest { workload_id: job })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output, output)?;
        }
        Command::Enable { workload } => {
            let response = client
                .enable(EnableRequest {
                    workload_id: workload,
                })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output, output)?;
        }
        Command::Disable { workload } => {
            let response = client
                .disable(DisableRequest {
                    workload_id: workload,
                })
                .await?
                .into_inner();
            print_mutation(response.changed, response.workload, json_output, output)?;
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
                output.executions(&executions)?;
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
                output.execution(&execution)?;
            }
        }
        Command::Logs {
            workload,
            execution,
            attempt,
            stream,
            follow,
            timestamps,
            prefix,
        } => {
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
                if json_output {
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
                    output.log_record(&record, LogOptions { timestamps, prefix })?;
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
    output: &mut HumanOutput,
) -> Result<(), Box<dyn Error>> {
    let workload = workload.ok_or("controller returned no workload")?;
    if json_output {
        println!(
            "{}",
            json!({ "changed": changed, "workload": workload_json(&workload) })
        );
    } else {
        output.mutation(changed, &workload)?;
    }
    Ok(())
}

fn print_workloads_json(workloads: &[Workload]) {
    println!(
        "{}",
        serde_json::Value::Array(workloads.iter().map(workload_json).collect())
    );
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
