#![cfg(windows)]

mod controller_link;
mod job;
mod journal;
mod process;
mod runner;

use std::{error::Error, path::PathBuf};

use clap::{Parser, Subcommand};
use susm_diagnostics::{Component, init as init_diagnostics};

#[derive(Debug, Parser)]
#[command(name = "susm-supervisor", version, about = "SUSM workload supervisor")]
struct Arguments {
    #[command(subcommand)]
    command: SupervisorCommand,
}

#[derive(Debug, Subcommand)]
enum SupervisorCommand {
    Run {
        #[arg(long)]
        spec: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments = Arguments::parse();
    let result = match arguments.command {
        SupervisorCommand::Run { spec } => {
            let configuration = runner::load_configuration(&spec).await?;
            let local_app_data = std::env::var_os("LOCALAPPDATA")
                .filter(|value| !value.is_empty())
                .ok_or("LOCALAPPDATA is missing")?;
            let diagnostics = init_diagnostics(
                Component::Supervisor,
                &PathBuf::from(local_app_data)
                    .join("susm")
                    .join("diagnostics")
                    .join("supervisors")
                    .join(&configuration.workload_id)
                    .join(&configuration.execution_id),
            )?;
            let manager_session_id = configuration.manager_session_id.clone();
            let workload_id = configuration.workload_id.clone();
            let execution_id = configuration.execution_id.clone();
            tracing::info!(
                name = "supervisor_starting",
                manager_session_id = %manager_session_id,
                workload_id = %workload_id,
                execution_id = %execution_id,
                attempt = configuration.attempt,
            );
            let journal = journal::RuntimeJournal::open(std::path::Path::new(
                &configuration.runtime_journal,
            ))?;
            let recovery = journal.recovery();
            let identity = recovery.checkpoint.map_or_else(
                || runner::SupervisorIdentity {
                    id: uuid::Uuid::now_v7().to_string(),
                    incarnation: 1,
                },
                |checkpoint| runner::SupervisorIdentity {
                    id: checkpoint.supervisor_id,
                    incarnation: checkpoint.incarnation.saturating_add(1),
                },
            );
            let (stop, stop_receiver) = tokio::sync::watch::channel(false);
            let stop_guard = stop.clone();
            let (policy, policy_receiver) = tokio::sync::watch::channel(None);
            let (observations, observation_receiver) = tokio::sync::mpsc::unbounded_channel();
            let version_pin = configuration.version_pin.clone();
            let link_configuration = configuration.clone();
            let link_identity = identity.clone();
            let link_journal = journal.clone();
            let mut link = tokio::spawn(async move {
                controller_link::run(
                    link_configuration,
                    link_identity,
                    link_journal,
                    stop,
                    policy,
                    observation_receiver,
                )
                .await
            });
            let attempt = runner::run(
                configuration,
                journal.clone(),
                identity,
                stop_receiver,
                policy_receiver,
                observations,
            )
            .await?;
            drop(stop_guard);
            if tokio::time::timeout(std::time::Duration::from_secs(5), &mut link)
                .await
                .is_err()
            {
                link.abort();
                let _ = link.await;
            }
            journal.finalize()?;
            if !version_pin.is_empty() {
                let _ = std::fs::remove_file(version_pin);
            }
            tracing::info!(
                name = "supervisor_stopped",
                manager_session_id = %manager_session_id,
                workload_id = %workload_id,
                execution_id = %execution_id,
                outcome = %attempt.outcome,
                exit_code = ?attempt.exit_code,
            );
            diagnostics.shutdown();
            attempt
        }
    };
    let code = result.exit_code.unwrap_or(1);
    std::process::exit(i32::try_from(code.min(255)).expect("bounded exit code fits i32"));
}
