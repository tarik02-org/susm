use std::{
    collections::BTreeMap,
    os::windows::process::CommandExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use prost::Message;
use sha2::{Digest, Sha256};
use susm_config::{
    LoggingPolicy, RetentionDuration, RetentionLimit, StopDefinition, WorkloadDefinition,
    WorkloadKind, load_directory,
};
use susm_domain::restart::{
    ExecutionRestartPolicy, JobRestartPolicy, ServiceRestartPolicy, ServiceRetryLimit,
};
use susm_protocol::{
    control::{Execution, WatchWorkloadsResponse, Workload},
    supervisor::{AttemptResult, EnvironmentValue, ExecutionConfiguration, PolicyUpdate},
};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{broadcast, watch},
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::storage::{
    ObservationUpdate, ProgressObservation, Storage, StoredExecution, StoredWorkload,
    TerminalObservation,
};

#[derive(Clone, Debug)]
pub struct ManagerPaths {
    pub config_directory: PathBuf,
    pub data_directory: PathBuf,
    pub supervisor_executable: PathBuf,
    pub manager_session_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error("configuration reload failed: {0}")]
    Config(#[from] susm_config::LoadError),
    #[error(transparent)]
    Storage(#[from] crate::storage::StorageError),
    #[error("workload '{0}' does not exist")]
    NotFound(String),
    #[error("workload '{0}' has no accepted definition")]
    DefinitionMissing(String),
    #[error("workload '{id}' is a {actual}, not a {expected}")]
    WrongKind {
        id: String,
        expected: &'static str,
        actual: String,
    },
    #[error("workload '{0}' is already active")]
    AlreadyActive(String),
    #[error("supervisor execution identity does not match durable state")]
    SupervisorIdentityMismatch,
    #[error("the manager session is ending")]
    SessionEnding,
    #[error("cannot change active workload '{0}' between service and job")]
    KindChangeActive(String),
    #[error("cannot encode execution snapshot: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("cannot decode stored execution snapshot: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("system time is before the Unix epoch")]
    InvalidSystemTime,
}

#[derive(Clone)]
pub struct Manager(Arc<Inner>);

struct Inner {
    paths: ManagerPaths,
    storage: Storage,
    state: Mutex<State>,
    changes: watch::Sender<u64>,
}

struct State {
    workloads: BTreeMap<String, WorkloadRecord>,
    active: BTreeMap<String, ActiveExecution>,
    revision: u64,
    session_ending: bool,
}

#[derive(Clone)]
struct WorkloadRecord {
    kind: String,
    definition: Option<ExecutionConfiguration>,
    enabled: bool,
    last_outcome: String,
}

struct ActiveExecution {
    execution_id: String,
    supervisor_process_id: u32,
    workload_process_id: u32,
    attempt: u32,
    error: String,
    state: &'static str,
    cancellation: CancellationToken,
    commands: broadcast::Sender<ExecutionCommand>,
    locally_owned: bool,
    restart_after: bool,
    process_definition_hash: Vec<u8>,
    policy_hash: Vec<u8>,
    restart_required: bool,
    policy_sync_pending: bool,
    attachment_id: Option<String>,
}

#[derive(Clone)]
pub struct ExecutionCommand {
    pub kind: String,
    pub payload: Vec<u8>,
}

pub struct SupervisorObservation {
    pub sequence: u64,
    pub kind: String,
    pub exit_code: Option<u32>,
    pub detail: String,
    pub attempt: u32,
    pub workload_process_id: u32,
}

pub struct SupervisorAttachment {
    pub configuration: ExecutionConfiguration,
    pub commands: broadcast::Receiver<ExecutionCommand>,
    pub committed_sequence: u64,
    pub attachment_id: String,
}

struct ExecutionFinish {
    state: String,
    exit_code: Option<u32>,
    error: String,
    supervisor_process_id: u32,
}

impl Manager {
    pub fn open(paths: ManagerPaths) -> Result<Self, ManagerError> {
        std::fs::create_dir_all(&paths.data_directory).map_err(|source| ManagerError::Io {
            path: paths.data_directory.clone(),
            source,
        })?;
        let storage = Storage::open(paths.data_directory.join("data").join("state.db"))?;
        let mut workloads = BTreeMap::new();
        for stored in storage.load_workloads()? {
            let definition = stored
                .definition
                .map(|bytes| ExecutionConfiguration::decode(bytes.as_slice()))
                .transpose()?;
            workloads.insert(
                stored.id,
                WorkloadRecord {
                    kind: stored.kind,
                    definition,
                    enabled: stored.enabled,
                    last_outcome: stored.last_outcome,
                },
            );
        }
        let mut active = BTreeMap::new();
        for execution in storage.load_active_executions()? {
            let configuration = ExecutionConfiguration::decode(execution.snapshot.as_slice())?;
            let current_session = paths.manager_session_id.as_deref().unwrap_or_default();
            if configuration.manager_session_id != current_session {
                let outcome = if configuration.kind == "job" {
                    "outcome-unknown"
                } else {
                    "stopped"
                };
                let workload_id = execution.workload_id.clone();
                tracing::warn!(
                    name = "stale_manager_session_execution_closed",
                    manager_session_id = %configuration.manager_session_id,
                    current_manager_session_id = %current_session,
                    workload_id = %execution.workload_id,
                    execution_id = %execution.id,
                    outcome,
                );
                storage.finish_execution(StoredExecution {
                    state: outcome.to_owned(),
                    supervisor_process_id: 0,
                    workload_process_id: 0,
                    ended_unix_ms: unix_ms()?,
                    exit_code: None,
                    error: "execution belonged to an ended manager session".to_owned(),
                    ..execution
                })?;
                if let Some(workload) = workloads.get_mut(&workload_id) {
                    workload.last_outcome = outcome.to_owned();
                }
                remove_version_pin(&configuration);
                continue;
            }
            let (commands, _) = broadcast::channel(16);
            active.insert(
                execution.workload_id,
                ActiveExecution {
                    execution_id: execution.id,
                    supervisor_process_id: execution.supervisor_process_id,
                    workload_process_id: execution.workload_process_id,
                    attempt: execution.attempt,
                    error: execution.error,
                    state: if execution.state == "stopping" {
                        "stopping"
                    } else {
                        "recovering"
                    },
                    cancellation: CancellationToken::new(),
                    commands,
                    locally_owned: false,
                    restart_after: false,
                    process_definition_hash: configuration.process_definition_hash.clone(),
                    policy_hash: policy_update(&configuration).policy_hash,
                    restart_required: false,
                    policy_sync_pending: false,
                    attachment_id: None,
                },
            );
        }
        let (changes, _) = watch::channel(0);
        Ok(Self(Arc::new(Inner {
            paths,
            storage,
            state: Mutex::new(State {
                workloads,
                active,
                revision: 0,
                session_ending: false,
            }),
            changes,
        })))
    }

    pub fn config_directory(&self) -> &Path {
        &self.0.paths.config_directory
    }

    pub fn data_directory(&self) -> &Path {
        &self.0.paths.data_directory
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.changes.subscribe()
    }

    pub fn reload(&self) -> Result<(bool, String), ManagerError> {
        let candidate = load_directory(&self.0.paths.config_directory)?;
        let generation = candidate.generation();
        let definitions = candidate
            .into_definitions()
            .into_values()
            .map(definition_snapshot)
            .collect::<Vec<_>>();
        let stored = definitions
            .iter()
            .map(|definition| {
                let mut bytes = Vec::new();
                definition.encode(&mut bytes)?;
                Ok(StoredWorkload {
                    id: definition.workload_id.clone(),
                    kind: definition.kind.clone(),
                    definition: Some(bytes),
                    enabled: false,
                    last_outcome: String::new(),
                })
            })
            .collect::<Result<Vec<_>, prost::EncodeError>>()?;
        {
            let state = self.0.state.lock().expect("manager state lock poisoned");
            for definition in &definitions {
                if state.active.contains_key(&definition.workload_id)
                    && state
                        .workloads
                        .get(&definition.workload_id)
                        .is_some_and(|current| current.kind != definition.kind)
                {
                    return Err(ManagerError::KindChangeActive(
                        definition.workload_id.clone(),
                    ));
                }
            }
        }
        let changed = self
            .0
            .storage
            .replace_definitions(generation.as_bytes().to_vec(), stored)?;
        if changed {
            let persisted = self.0.storage.load_workloads()?;
            let mut state = self.0.state.lock().expect("manager state lock poisoned");
            for item in persisted {
                let definition = item
                    .definition
                    .map(|bytes| ExecutionConfiguration::decode(bytes.as_slice()))
                    .transpose()?;
                state.workloads.insert(
                    item.id,
                    WorkloadRecord {
                        kind: item.kind,
                        definition,
                        enabled: item.enabled,
                        last_outcome: item.last_outcome,
                    },
                );
            }
            reconcile_reload(&mut state);
            self.0.bump(&mut state);
        } else {
            let mut state = self.0.state.lock().expect("manager state lock poisoned");
            if reconcile_reload(&mut state) {
                self.0.bump(&mut state);
            }
        }
        tracing::info!(
            name = "configuration_reloaded",
            changed,
            generation = %hex(generation.as_bytes()),
        );
        Ok((changed, hex(generation.as_bytes())))
    }

    pub fn list_workloads(&self) -> Vec<Workload> {
        let state = self.0.state.lock().expect("manager state lock poisoned");
        state
            .workloads
            .iter()
            .map(|(id, record)| workload_view(id, record, state.active.get(id)))
            .collect()
    }

    pub fn workload(&self, id: &str) -> Result<Workload, ManagerError> {
        let state = self.0.state.lock().expect("manager state lock poisoned");
        let record = state
            .workloads
            .get(id)
            .ok_or_else(|| ManagerError::NotFound(id.to_owned()))?;
        Ok(workload_view(id, record, state.active.get(id)))
    }

    pub fn snapshot(&self) -> WatchWorkloadsResponse {
        let state = self.0.state.lock().expect("manager state lock poisoned");
        WatchWorkloadsResponse {
            revision: state.revision,
            workloads: state
                .workloads
                .iter()
                .map(|(id, record)| workload_view(id, record, state.active.get(id)))
                .collect(),
        }
    }

    pub fn start_service(&self, id: &str) -> Result<(bool, Workload), ManagerError> {
        self.start(id, "service", false, None)
    }

    pub fn run_job(&self, id: &str) -> Result<(bool, Workload), ManagerError> {
        self.start(id, "job", true, None)
    }

    pub fn stop_service(&self, id: &str) -> Result<(bool, Workload), ManagerError> {
        self.cancel(id, "service", false)
    }

    pub fn cancel_job(&self, id: &str) -> Result<(bool, Workload), ManagerError> {
        self.cancel(id, "job", false)
    }

    pub fn restart_service(&self, id: &str) -> Result<(bool, Workload), ManagerError> {
        self.cancel(id, "service", true)
    }

    pub fn rerun_job(&self, id: &str) -> Result<(bool, Workload), ManagerError> {
        let active = {
            let state = self.0.state.lock().expect("manager state lock poisoned");
            state.active.contains_key(id)
        };
        if active {
            self.cancel(id, "job", true)
        } else {
            self.start(id, "job", true, None)
        }
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(bool, Workload), ManagerError> {
        let changed = self.0.storage.set_enabled(id.to_owned(), enabled)?;
        let mut state = self.0.state.lock().expect("manager state lock poisoned");
        let record = state
            .workloads
            .get_mut(id)
            .ok_or_else(|| ManagerError::NotFound(id.to_owned()))?;
        if changed {
            record.enabled = enabled;
            self.0.bump(&mut state);
            tracing::info!(
                name = "workload_enablement_changed",
                workload_id = %id,
                enabled,
            );
        }
        let record = state.workloads.get(id).expect("workload checked above");
        Ok((changed, workload_view(id, record, state.active.get(id))))
    }

    pub fn list_executions(
        &self,
        workload_id: &str,
        limit: u32,
        before: Option<i64>,
    ) -> Result<Vec<Execution>, ManagerError> {
        if !self
            .0
            .state
            .lock()
            .expect("manager state lock poisoned")
            .workloads
            .contains_key(workload_id)
        {
            return Err(ManagerError::NotFound(workload_id.to_owned()));
        }
        Ok(self
            .0
            .storage
            .list_executions(workload_id.to_owned(), limit, before)?
            .into_iter()
            .map(execution_view)
            .collect())
    }

    pub fn execution(&self, id: &str) -> Result<Execution, ManagerError> {
        self.0
            .storage
            .get_execution(id.to_owned())?
            .map(execution_view)
            .ok_or_else(|| ManagerError::NotFound(id.to_owned()))
    }

    pub fn start_enabled(&self) {
        let (services, jobs) = {
            let state = self.0.state.lock().expect("manager state lock poisoned");
            let services = state
                .workloads
                .iter()
                .filter(|(_, record)| record.enabled && record.kind == "service")
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            let jobs = state
                .workloads
                .iter()
                .filter(|(_, record)| record.enabled && record.kind == "job")
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            (services, jobs)
        };
        for id in services {
            let _ = self.start_service(&id);
        }
        let Some(manager_session_id) = self
            .0
            .paths
            .manager_session_id
            .as_ref()
            .filter(|value| !value.is_empty())
        else {
            return;
        };
        for id in jobs {
            if let Err(error) = self.start(&id, "job", true, Some(manager_session_id.as_str())) {
                tracing::warn!(
                    name = "enabled_job_start_failed",
                    manager_session_id = %manager_session_id,
                    workload_id = %id,
                    error = %error,
                );
            }
        }
    }

    pub fn end_session(&self) {
        let mut state = self.0.state.lock().expect("manager state lock poisoned");
        if state.session_ending {
            return;
        }
        state.session_ending = true;
        for active in state.active.values_mut() {
            active.restart_after = false;
            active.state = "stopping";
            active.cancellation.cancel();
            let _ = active.commands.send(ExecutionCommand {
                kind: "session-ending".to_owned(),
                payload: Vec::new(),
            });
        }
        self.0.bump(&mut state);
    }

    pub fn is_quiescent(&self) -> bool {
        self.0
            .state
            .lock()
            .expect("manager state lock poisoned")
            .active
            .is_empty()
    }

    pub fn start_recovery_monitors(&self) {
        let executions = {
            let state = self.0.state.lock().expect("manager state lock poisoned");
            state
                .active
                .values()
                .filter(|active| !active.locally_owned)
                .map(|active| active.execution_id.clone())
                .collect::<Vec<_>>()
        };
        for execution_id in executions {
            let manager = self.clone();
            tokio::spawn(async move {
                let Some(stored) = manager
                    .0
                    .storage
                    .get_execution(execution_id.clone())
                    .ok()
                    .flatten()
                else {
                    return;
                };
                let Ok(definition) = ExecutionConfiguration::decode(stored.snapshot.as_slice())
                else {
                    return;
                };
                let root = manager
                    .0
                    .execution_runtime_root(&definition.manager_session_id, &execution_id);
                let mut replacement_at = Instant::now() + Duration::from_secs(3);
                let mut was_recovering = true;
                loop {
                    if let Some(result) = find_attempt_result(&root).await {
                        let _ = manager.supervisor_terminal(
                            &execution_id,
                            &result.outcome,
                            result.exit_code,
                            result.error,
                        );
                        return;
                    }
                    let replacement = {
                        let mut state =
                            manager.0.state.lock().expect("manager state lock poisoned");
                        let Some((workload_id, active)) = state
                            .active
                            .iter_mut()
                            .find(|(_, active)| active.execution_id == execution_id)
                        else {
                            return;
                        };
                        let recovering = active.state == "recovering";
                        if recovering && !was_recovering {
                            replacement_at = Instant::now() + Duration::from_secs(3);
                        }
                        was_recovering = recovering;
                        if recovering && Instant::now() >= replacement_at {
                            active.locally_owned = true;
                            active.state = "starting";
                            Some((workload_id.clone(), active.cancellation.clone()))
                        } else {
                            None
                        }
                    };
                    if let Some((workload_id, cancellation)) = replacement {
                        manager.0.spawn_execution(
                            workload_id,
                            execution_id.clone(),
                            definition.clone(),
                            cancellation,
                            stored.started_unix_ms,
                        );
                        return;
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            });
        }
    }

    pub fn supervisor_disconnected(&self, execution_id: &str, attachment_id: &str) {
        let mut state = self.0.state.lock().expect("manager state lock poisoned");
        let Some(active) = state
            .active
            .values_mut()
            .find(|active| active.execution_id == execution_id)
        else {
            return;
        };
        if active.locally_owned
            || active.state == "stopping"
            || active.attachment_id.as_deref() != Some(attachment_id)
        {
            return;
        }
        active.attachment_id = None;
        active.state = "recovering";
        self.0.bump(&mut state);
    }

    pub fn attach_supervisor(
        &self,
        execution_id: &str,
        workload_id: &str,
        manager_session_id: &str,
        execution_config_hash: &[u8],
        supervisor_process_id: u32,
    ) -> Result<SupervisorAttachment, ManagerError> {
        let stored = self
            .0
            .storage
            .get_execution(execution_id.to_owned())?
            .ok_or_else(|| ManagerError::NotFound(execution_id.to_owned()))?;
        let configuration = ExecutionConfiguration::decode(stored.snapshot.as_slice())?;
        if configuration.workload_id != workload_id
            || configuration.manager_session_id != manager_session_id
            || configuration.execution_config_hash != execution_config_hash
        {
            return Err(ManagerError::SupervisorIdentityMismatch);
        }
        let mut state = self.0.state.lock().expect("manager state lock poisoned");
        let current_definition = state
            .workloads
            .get(&stored.workload_id)
            .and_then(|workload| workload.definition.clone());
        let active = state
            .active
            .get_mut(&stored.workload_id)
            .filter(|active| active.execution_id == execution_id)
            .ok_or_else(|| ManagerError::NotFound(execution_id.to_owned()))?;
        if active.supervisor_process_id != 0
            && active.supervisor_process_id != supervisor_process_id
        {
            return Err(ManagerError::SupervisorIdentityMismatch);
        }
        if active.locally_owned && active.supervisor_process_id == 0 {
            return Err(ManagerError::SupervisorIdentityMismatch);
        }
        let locally_owned = active.locally_owned;
        active.supervisor_process_id = supervisor_process_id;
        if locally_owned {
            active.state = "launching";
        } else {
            active.state = active_state(&stored.state);
            active.workload_process_id = stored.workload_process_id;
            active.attempt = stored.attempt;
            active.error.clone_from(&stored.error);
        }
        let attachment_id = Uuid::now_v7().to_string();
        active.attachment_id = Some(attachment_id.clone());
        let commands = active.commands.subscribe();
        if active.policy_sync_pending
            && let Some(definition) = current_definition
        {
            let update = policy_update(&definition);
            let _ = active.commands.send(ExecutionCommand {
                kind: "update-policy".to_owned(),
                payload: update.encode_to_vec(),
            });
        }
        if locally_owned {
            self.0
                .storage
                .set_execution_supervisor(execution_id.to_owned(), supervisor_process_id)?;
        }
        self.0.bump(&mut state);
        Ok(SupervisorAttachment {
            configuration,
            commands,
            committed_sequence: stored.committed_sequence,
            attachment_id,
        })
    }

    pub fn record_supervisor_observation(
        &self,
        execution_id: &str,
        observation: SupervisorObservation,
    ) -> Result<(), ManagerError> {
        let SupervisorObservation {
            sequence,
            kind,
            exit_code,
            detail,
            attempt,
            workload_process_id,
        } = observation;
        let terminal = matches!(
            kind.as_str(),
            "completed" | "failed" | "stopped" | "cancelled" | "outcome-unknown"
        );
        let stored = self
            .0
            .storage
            .get_execution(execution_id.to_owned())?
            .ok_or_else(|| ManagerError::NotFound(execution_id.to_owned()))?;
        let terminal_update = if terminal {
            let state = self.0.state.lock().expect("manager state lock poisoned");
            state
                .active
                .get(&stored.workload_id)
                .filter(|active| active.execution_id == execution_id && !active.locally_owned)
                .map(|_| TerminalObservation {
                    state: kind.clone(),
                    ended_unix_ms: unix_ms().unwrap_or(stored.started_unix_ms),
                    exit_code,
                    error: detail.clone(),
                    attempt,
                })
        } else {
            None
        };
        let terminalized = terminal_update.is_some();
        let progress = progress_observation(&kind, attempt, workload_process_id, &detail);
        let update = terminal_update.map_or_else(
            || {
                progress
                    .as_ref()
                    .map_or(ObservationUpdate::Acknowledge, |progress| {
                        ObservationUpdate::Progress(ProgressObservation {
                            state: progress.state.to_owned(),
                            workload_process_id: progress.workload_process_id,
                            attempt: progress.attempt,
                            error: progress.error.clone(),
                        })
                    })
            },
            ObservationUpdate::Terminal,
        );
        let committed =
            self.0
                .storage
                .commit_observation(execution_id.to_owned(), sequence, update)?;
        let mut restart = false;
        if committed && terminalized {
            remove_version_pin(&ExecutionConfiguration::decode(stored.snapshot.as_slice())?);
            let mut state = self.0.state.lock().expect("manager state lock poisoned");
            if state
                .active
                .get(&stored.workload_id)
                .is_some_and(|active| active.execution_id == execution_id)
            {
                restart = !state.session_ending
                    && state
                        .active
                        .get(&stored.workload_id)
                        .is_some_and(|active| active.restart_after);
                state.active.remove(&stored.workload_id);
                if let Some(workload) = state.workloads.get_mut(&stored.workload_id) {
                    workload.last_outcome.clone_from(&kind);
                }
                self.0.bump(&mut state);
            }
        } else if committed {
            let mut state = self.0.state.lock().expect("manager state lock poisoned");
            let active = state
                .active
                .get_mut(&stored.workload_id)
                .filter(|active| active.execution_id == execution_id);
            let mut changed = false;
            if let Some(active) = active {
                if kind == "policy-applied"
                    && detail == hex(&active.policy_hash)
                    && active.policy_sync_pending
                {
                    active.policy_sync_pending = false;
                    changed = true;
                }
                if let Some(progress) = progress {
                    active.state = progress.state;
                    active.workload_process_id = progress.workload_process_id;
                    active.attempt = progress.attempt;
                    if let Some(error) = progress.error {
                        active.error = error;
                    }
                    changed = true;
                }
            }
            if changed {
                self.0.bump(&mut state);
            }
        }
        if restart {
            self.start_replacement(&stored.workload_id);
        }
        Ok(())
    }

    pub fn supervisor_terminal(
        &self,
        execution_id: &str,
        outcome: &str,
        exit_code: Option<u32>,
        error: String,
    ) -> Result<(), ManagerError> {
        let stored = self
            .0
            .storage
            .get_execution(execution_id.to_owned())?
            .ok_or_else(|| ManagerError::NotFound(execution_id.to_owned()))?;
        let definition = ExecutionConfiguration::decode(stored.snapshot.as_slice())?;
        let mut state = self.0.state.lock().expect("manager state lock poisoned");
        let Some(active) = state.active.get(&stored.workload_id) else {
            return Ok(());
        };
        if active.execution_id != execution_id || active.locally_owned {
            return Ok(());
        }
        let stopping = active.state == "stopping";
        let restart = !state.session_ending && active.restart_after;
        state.active.remove(&stored.workload_id);
        let state_name = if stopping {
            if definition.kind == "job" {
                "cancelled"
            } else {
                "stopped"
            }
        } else if matches!(
            outcome,
            "completed" | "failed" | "stopped" | "cancelled" | "outcome-unknown"
        ) {
            outcome
        } else if exit_code.is_some_and(|code| definition.success_exit_codes.contains(&code)) {
            "completed"
        } else {
            "failed"
        };
        if let Some(workload) = state.workloads.get_mut(&stored.workload_id) {
            workload.last_outcome = state_name.to_owned();
        }
        self.0.bump(&mut state);
        drop(state);
        let replacement = restart.then(|| stored.workload_id.clone());
        self.0.storage.finish_execution(StoredExecution {
            state: state_name.to_owned(),
            workload_process_id: 0,
            ended_unix_ms: unix_ms()?,
            exit_code,
            error,
            ..stored
        })?;
        remove_version_pin(&definition);
        if let Some(workload_id) = replacement {
            self.start_replacement(&workload_id);
        }
        Ok(())
    }

    fn start_replacement(&self, workload_id: &str) {
        let kind = {
            let state = self.0.state.lock().expect("manager state lock poisoned");
            state
                .workloads
                .get(workload_id)
                .map(|record| record.kind.clone())
        };
        let result = match kind.as_deref() {
            Some("service") => self.start_service(workload_id),
            Some("job") => self.run_job(workload_id),
            _ => return,
        };
        if let Err(error) = result {
            tracing::error!(
                name = "execution_replacement_failed",
                workload_id,
                error = %error,
            );
        }
    }

    fn start(
        &self,
        id: &str,
        expected_kind: &'static str,
        exclusive: bool,
        enabled_manager_session: Option<&str>,
    ) -> Result<(bool, Workload), ManagerError> {
        let mut state = self.0.state.lock().expect("manager state lock poisoned");
        if state.session_ending {
            return Err(ManagerError::SessionEnding);
        }
        let record = state
            .workloads
            .get(id)
            .ok_or_else(|| ManagerError::NotFound(id.to_owned()))?;
        ensure_kind(id, record, expected_kind)?;
        let mut definition = record
            .definition
            .clone()
            .ok_or_else(|| ManagerError::DefinitionMissing(id.to_owned()))?;
        if state.active.contains_key(id) {
            if exclusive {
                return Err(ManagerError::AlreadyActive(id.to_owned()));
            }
            return Ok((false, workload_view(id, record, state.active.get(id))));
        }
        let execution_id = Uuid::now_v7().to_string();
        definition.execution_id = execution_id.clone();
        definition.manager_session_id = self.0.paths.manager_session_id.clone().unwrap_or_default();
        prepare_execution_build(
            &mut definition,
            &self.0.paths.supervisor_executable,
            &execution_id,
        )?;
        let cancellation = CancellationToken::new();
        let (commands, _) = broadcast::channel(16);
        let started = unix_ms()?;
        state.active.insert(
            id.to_owned(),
            ActiveExecution {
                execution_id: execution_id.clone(),
                supervisor_process_id: 0,
                workload_process_id: 0,
                attempt: 0,
                error: String::new(),
                state: "starting",
                cancellation: cancellation.clone(),
                commands,
                locally_owned: true,
                restart_after: false,
                process_definition_hash: definition.process_definition_hash.clone(),
                policy_hash: policy_update(&definition).policy_hash,
                restart_required: false,
                policy_sync_pending: false,
                attachment_id: None,
            },
        );
        let execution = StoredExecution {
            id: execution_id.clone(),
            workload_id: id.to_owned(),
            state: "starting".to_owned(),
            supervisor_process_id: 0,
            workload_process_id: 0,
            attempt: 0,
            started_unix_ms: started,
            ended_unix_ms: 0,
            exit_code: None,
            error: String::new(),
            snapshot: definition.encode_to_vec(),
            committed_sequence: 0,
        };
        let insert = match enabled_manager_session {
            Some(manager_session_id) => self
                .0
                .storage
                .insert_enabled_job_execution(execution, manager_session_id.to_owned()),
            None => self.0.storage.insert_execution(execution).map(|()| true),
        };
        let inserted = match insert {
            Ok(inserted) => inserted,
            Err(error) => {
                state.active.remove(id);
                remove_version_pin(&definition);
                return Err(error.into());
            }
        };
        if !inserted {
            state.active.remove(id);
            remove_version_pin(&definition);
            let record = state.workloads.get(id).expect("workload checked above");
            return Ok((false, workload_view(id, record, None)));
        }
        self.0.bump(&mut state);
        let view = workload_view(
            id,
            state.workloads.get(id).expect("workload checked above"),
            state.active.get(id),
        );
        drop(state);
        self.0.spawn_execution(
            id.to_owned(),
            execution_id.clone(),
            definition,
            cancellation,
            started,
        );
        tracing::info!(
            name = "execution_started",
            workload_id = %id,
            execution_id = %execution_id,
            kind = expected_kind,
        );
        Ok((true, view))
    }

    fn cancel(
        &self,
        id: &str,
        expected_kind: &'static str,
        restart_after: bool,
    ) -> Result<(bool, Workload), ManagerError> {
        let mut state = self.0.state.lock().expect("manager state lock poisoned");
        let record = state
            .workloads
            .get(id)
            .ok_or_else(|| ManagerError::NotFound(id.to_owned()))?
            .clone();
        ensure_kind(id, &record, expected_kind)?;
        let Some(active) = state.active.get_mut(id) else {
            return Ok((false, workload_view(id, &record, None)));
        };
        active.restart_after |= restart_after;
        active.state = "stopping";
        active.cancellation.cancel();
        let _ = active.commands.send(ExecutionCommand {
            kind: "stop".to_owned(),
            payload: Vec::new(),
        });
        tracing::info!(
            name = "execution_stop_requested",
            workload_id = %id,
            execution_id = %active.execution_id,
            restart_after,
        );
        self.0.bump(&mut state);
        let record = state.workloads.get(id).expect("workload checked above");
        Ok((true, workload_view(id, record, state.active.get(id))))
    }
}

impl Inner {
    fn bump(&self, state: &mut State) {
        state.revision = state.revision.saturating_add(1);
        self.changes.send_replace(state.revision);
    }

    fn spawn_execution(
        self: &Arc<Self>,
        workload_id: String,
        execution_id: String,
        definition: ExecutionConfiguration,
        cancellation: CancellationToken,
        started: i64,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            let finish = manager
                .run_execution(&workload_id, &execution_id, definition, cancellation)
                .await;
            manager.complete_execution(&workload_id, &execution_id, started, finish);
        });
    }

    async fn run_execution(
        &self,
        workload_id: &str,
        execution_id: &str,
        mut definition: ExecutionConfiguration,
        cancellation: CancellationToken,
    ) -> ExecutionFinish {
        definition.execution_id = execution_id.to_owned();
        definition.attempt = 1;
        let execution_root =
            self.execution_runtime_root(&definition.manager_session_id, execution_id);
        if let Err(error) = tokio::fs::create_dir_all(&execution_root).await {
            return io_finish("failed", 0, &execution_root, error);
        }
        let spec_path = execution_root.join("execution.pb");
        let result_path = execution_root.join("result.pb");
        definition.result_file = result_path.to_string_lossy().into_owned();
        definition.runtime_journal = execution_root
            .join("runtime.susm-runtime.open")
            .to_string_lossy()
            .into_owned();
        definition.log_directory = self
            .paths
            .data_directory
            .join("logs")
            .join(workload_id)
            .join(execution_id)
            .to_string_lossy()
            .into_owned();
        if let Err(error) = tokio::fs::write(&spec_path, definition.encode_to_vec()).await {
            return io_finish("failed", 0, &spec_path, error);
        }

        let mut supervisor_failures = 0_u32;
        let mut supervisor_process_id = 0;
        let (result, cancelled) = loop {
            if cancellation.is_cancelled() {
                break (stopped_attempt_result(), true);
            }
            let supervisor_executable = Path::new(&definition.supervisor_executable);
            let mut command = Command::new(supervisor_executable);
            command
                .arg("run")
                .arg("--spec")
                .arg(&spec_path)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            command.as_std_mut().creation_flags(CREATE_NO_WINDOW.0);
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    supervisor_failures = supervisor_failures.saturating_add(1);
                    if supervisor_failures >= 8 {
                        return io_finish(
                            "supervisor-unavailable",
                            0,
                            supervisor_executable,
                            error,
                        );
                    }
                    tokio::select! {
                        () = sleep(supervisor_retry_delay(supervisor_failures)) => {}
                        () = cancellation.cancelled() => {
                            break (stopped_attempt_result(), true);
                        }
                    }
                    continue;
                }
            };
            supervisor_process_id = child.id().unwrap_or(0);
            let _ = self
                .storage
                .set_execution_supervisor(execution_id.to_owned(), supervisor_process_id);
            {
                let mut state = self.state.lock().expect("manager state lock poisoned");
                if let Some(active) = state.active.get_mut(workload_id)
                    && active.execution_id == execution_id
                {
                    active.supervisor_process_id = supervisor_process_id;
                    active.state = "launching";
                    self.bump(&mut state);
                }
            }

            let cancelled = tokio::select! {
                () = cancellation.cancelled() => {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(b"stop\n").await;
                    }
                    let stop_wait = Duration::from_millis(
                        definition.stop_timeout_ms.saturating_add(5_000),
                    );
                    if timeout(stop_wait, child.wait()).await.is_err() {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                    }
                    true
                }
                _ = child.wait() => false,
            };
            if let Some(result) = read_attempt_result(&result_path).await {
                break (result, cancelled);
            }
            if cancelled {
                break (stopped_attempt_result(), true);
            }
            supervisor_failures = supervisor_failures.saturating_add(1);
            if supervisor_failures >= 8 {
                return ExecutionFinish {
                    state: "supervisor-unavailable".to_owned(),
                    exit_code: None,
                    error: "supervisor launch budget exhausted without a terminal result"
                        .to_owned(),
                    supervisor_process_id,
                };
            }
            tokio::select! {
                () = sleep(supervisor_retry_delay(supervisor_failures)) => {}
                () = cancellation.cancelled() => {
                    break (stopped_attempt_result(), true);
                }
            }
        };
        let successful = result
            .exit_code
            .is_some_and(|code| definition.success_exit_codes.contains(&code));
        ExecutionFinish {
            state: if matches!(
                result.outcome.as_str(),
                "completed" | "failed" | "stopped" | "cancelled" | "outcome-unknown"
            ) {
                result.outcome.clone()
            } else if cancelled || result.stop_requested {
                if definition.kind == "job" {
                    "cancelled".to_owned()
                } else {
                    "stopped".to_owned()
                }
            } else if successful {
                "completed".to_owned()
            } else {
                "failed".to_owned()
            },
            exit_code: result.exit_code,
            error: result.error,
            supervisor_process_id,
        }
    }

    fn complete_execution(
        self: &Arc<Self>,
        workload_id: &str,
        execution_id: &str,
        started: i64,
        finish: ExecutionFinish,
    ) {
        let ended = unix_ms().unwrap_or(started);
        tracing::info!(
            name = "execution_terminal",
            workload_id = %workload_id,
            execution_id = %execution_id,
            outcome = %finish.state,
            exit_code = ?finish.exit_code,
        );
        if let Ok(Some(mut execution)) = self.storage.get_execution(execution_id.to_owned()) {
            execution.state.clone_from(&finish.state);
            execution.supervisor_process_id = finish.supervisor_process_id;
            execution.workload_process_id = 0;
            execution.ended_unix_ms = ended;
            execution.exit_code = finish.exit_code;
            execution.error.clone_from(&finish.error);
            let _ = self.storage.finish_execution(execution);
        }
        if let Ok(Some(execution)) = self.storage.get_execution(execution_id.to_owned())
            && let Ok(configuration) = ExecutionConfiguration::decode(execution.snapshot.as_slice())
        {
            remove_version_pin(&configuration);
        }
        let restart = {
            let mut state = self.state.lock().expect("manager state lock poisoned");
            let restart = !state.session_ending
                && state.active.get(workload_id).is_some_and(|active| {
                    active.execution_id == execution_id && active.restart_after
                });
            if state
                .active
                .get(workload_id)
                .is_some_and(|active| active.execution_id == execution_id)
            {
                state.active.remove(workload_id);
            }
            if let Some(record) = state.workloads.get_mut(workload_id) {
                record.last_outcome = finish.state;
            }
            self.bump(&mut state);
            restart
        };
        if restart {
            Manager(self.clone()).start_replacement(workload_id);
        }
    }

    fn execution_runtime_root(&self, manager_session_id: &str, execution_id: &str) -> PathBuf {
        self.paths
            .data_directory
            .join("runtime")
            .join("sessions")
            .join(if manager_session_id.is_empty() {
                "standalone"
            } else {
                manager_session_id
            })
            .join("executions")
            .join(execution_id)
    }
}

fn definition_snapshot(definition: WorkloadDefinition) -> ExecutionConfiguration {
    let process = definition.process();
    let mut snapshot = ExecutionConfiguration {
        workload_id: definition.id().to_string(),
        kind: match definition.kind() {
            WorkloadKind::Service => "service",
            WorkloadKind::Job => "job",
        }
        .to_owned(),
        executable: process.executable().to_owned(),
        arguments: process.arguments().map(str::to_owned).collect(),
        working_directory: process.working_directory().to_owned(),
        environment_set: process
            .environment()
            .set()
            .iter()
            .map(|(name, value)| EnvironmentValue {
                name: name.to_string(),
                value: value.to_string(),
            })
            .collect(),
        environment_unset: process
            .environment()
            .unset()
            .iter()
            .map(ToString::to_string)
            .collect(),
        success_exit_codes: definition.success_exit_codes().iter().copied().collect(),
        ..ExecutionConfiguration::default()
    };
    snapshot.process_definition_hash = definition.process_hash().as_bytes().to_vec();
    apply_restart(&mut snapshot, definition.restart());
    apply_stop(&mut snapshot, definition.stop());
    apply_logging(&mut snapshot, definition.logging());
    set_execution_hash(&mut snapshot);
    snapshot
}

fn prepare_execution_build(
    configuration: &mut ExecutionConfiguration,
    supervisor_executable: &Path,
    execution_id: &str,
) -> Result<(), ManagerError> {
    configuration.supervisor_executable = supervisor_executable.to_string_lossy().into_owned();
    if let Some(pin) = version_pin_path(supervisor_executable, execution_id) {
        let parent = pin.parent().expect("version pin has a parent");
        std::fs::create_dir_all(parent).map_err(|source| ManagerError::Io {
            path: parent.to_owned(),
            source,
        })?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&pin)
            .map_err(|source| ManagerError::Io {
                path: pin.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| ManagerError::Io {
            path: pin.clone(),
            source,
        })?;
        configuration.version_pin = pin.to_string_lossy().into_owned();
    }
    set_execution_hash(configuration);
    Ok(())
}

fn version_pin_path(supervisor_executable: &Path, execution_id: &str) -> Option<PathBuf> {
    let version = supervisor_executable.parent()?;
    let versions = version.parent()?;
    if versions
        .file_name()
        .is_none_or(|name| !name.eq_ignore_ascii_case("versions"))
    {
        return None;
    }
    let installation = versions.parent()?;
    Some(
        installation
            .join("pins")
            .join(version.file_name()?)
            .join(format!("{execution_id}.pin")),
    )
}

fn remove_version_pin(configuration: &ExecutionConfiguration) {
    if !configuration.version_pin.is_empty() {
        let _ = std::fs::remove_file(&configuration.version_pin);
    }
}

fn set_execution_hash(configuration: &mut ExecutionConfiguration) {
    configuration.execution_config_hash.clear();
    configuration.execution_config_hash = Sha256::digest(configuration.encode_to_vec()).to_vec();
}

fn policy_update(configuration: &ExecutionConfiguration) -> PolicyUpdate {
    let mut update = PolicyUpdate {
        success_exit_codes: configuration.success_exit_codes.clone(),
        restart_policy: configuration.restart_policy.clone(),
        max_restarts: configuration.max_restarts,
        max_restarts_unlimited: configuration.max_restarts_unlimited,
        restart_reset_after_ms: configuration.restart_reset_after_ms,
        restart_backoff_initial_ms: configuration.restart_backoff_initial_ms,
        restart_backoff_multiplier: configuration.restart_backoff_multiplier,
        restart_backoff_maximum_ms: configuration.restart_backoff_maximum_ms,
        stop_method: configuration.stop_method.clone(),
        stop_timeout_ms: configuration.stop_timeout_ms,
        stop_executable: configuration.stop_executable.clone(),
        stop_arguments: configuration.stop_arguments.clone(),
        capture_logs: configuration.capture_logs,
        segment_size: configuration.segment_size,
        segment_age_ms: configuration.segment_age_ms,
        retention_size: configuration.retention_size,
        retention_size_unlimited: configuration.retention_size_unlimited,
        retention_age_ms: configuration.retention_age_ms,
        retention_age_unlimited: configuration.retention_age_unlimited,
        policy_hash: Vec::new(),
    };
    update.policy_hash = Sha256::digest(update.encode_to_vec()).to_vec();
    update
}

fn reconcile_reload(state: &mut State) -> bool {
    let mut changed = false;
    let active_ids = state.active.keys().cloned().collect::<Vec<_>>();
    for id in active_ids {
        let definition = state
            .workloads
            .get(&id)
            .and_then(|workload| workload.definition.clone());
        let Some(active) = state.active.get_mut(&id) else {
            continue;
        };
        let Some(definition) = definition else {
            if active.restart_required {
                active.restart_required = false;
                changed = true;
            }
            continue;
        };
        let restart_required = active.process_definition_hash != definition.process_definition_hash;
        if active.restart_required != restart_required {
            active.restart_required = restart_required;
            changed = true;
        }
        let update = policy_update(&definition);
        if active.policy_hash != update.policy_hash {
            active.policy_hash.clone_from(&update.policy_hash);
            active.policy_sync_pending = true;
            changed = true;
        }
        if active.policy_sync_pending {
            let _ = active.commands.send(ExecutionCommand {
                kind: "update-policy".to_owned(),
                payload: update.encode_to_vec(),
            });
        }
    }
    changed
}

fn apply_restart(snapshot: &mut ExecutionConfiguration, restart: ExecutionRestartPolicy) {
    match restart {
        ExecutionRestartPolicy::Service(ServiceRestartPolicy::Never)
        | ExecutionRestartPolicy::Job(JobRestartPolicy::Never) => {
            snapshot.restart_policy = "never".to_owned();
        }
        ExecutionRestartPolicy::Service(
            ServiceRestartPolicy::OnFailure(policy) | ServiceRestartPolicy::Always(policy),
        ) => {
            snapshot.restart_policy = if matches!(
                restart,
                ExecutionRestartPolicy::Service(ServiceRestartPolicy::Always(_))
            ) {
                "always"
            } else {
                "on-failure"
            }
            .to_owned();
            match policy.limit() {
                ServiceRetryLimit::Finite(limit) => snapshot.max_restarts = limit.get(),
                ServiceRetryLimit::Unlimited => snapshot.max_restarts_unlimited = true,
            }
            snapshot.restart_reset_after_ms = duration_ms(policy.reset_after().get());
            apply_backoff(snapshot, policy.backoff());
        }
        ExecutionRestartPolicy::Job(JobRestartPolicy::OnFailure(policy)) => {
            snapshot.restart_policy = "on-failure".to_owned();
            snapshot.max_restarts = policy.max_restarts().get();
            apply_backoff(snapshot, policy.backoff());
        }
    }
}

fn apply_backoff(
    snapshot: &mut ExecutionConfiguration,
    policy: susm_domain::restart::BackoffPolicy,
) {
    snapshot.restart_backoff_initial_ms = duration_ms(policy.initial().get());
    snapshot.restart_backoff_multiplier = policy.multiplier().get();
    snapshot.restart_backoff_maximum_ms = duration_ms(policy.maximum().get());
}

fn apply_stop(snapshot: &mut ExecutionConfiguration, stop: &StopDefinition) {
    match stop {
        StopDefinition::CtrlBreak { timeout } => {
            snapshot.stop_method = "ctrl-break".to_owned();
            snapshot.stop_timeout_ms = duration_ms(*timeout);
        }
        StopDefinition::Command { timeout, command } => {
            snapshot.stop_method = "command".to_owned();
            snapshot.stop_timeout_ms = duration_ms(*timeout);
            snapshot.stop_executable = command.executable().to_owned();
            snapshot.stop_arguments = command.arguments().map(str::to_owned).collect();
        }
        StopDefinition::Kill => snapshot.stop_method = "kill".to_owned(),
    }
}

fn apply_logging(snapshot: &mut ExecutionConfiguration, logging: LoggingPolicy) {
    match logging {
        LoggingPolicy::Disabled => snapshot.capture_logs = false,
        LoggingPolicy::Capture {
            segment_size,
            segment_age,
            retention_size,
            retention_age,
        } => {
            snapshot.capture_logs = true;
            snapshot.segment_size = segment_size;
            snapshot.segment_age_ms = duration_ms(segment_age);
            match retention_size {
                RetentionLimit::Limited(value) => snapshot.retention_size = value,
                RetentionLimit::Unlimited => snapshot.retention_size_unlimited = true,
            }
            match retention_age {
                RetentionDuration::Limited(value) => {
                    snapshot.retention_age_ms = duration_ms(value);
                }
                RetentionDuration::Unlimited => snapshot.retention_age_unlimited = true,
            }
        }
    }
}

async fn read_attempt_result(path: &Path) -> Option<AttemptResult> {
    let bytes = tokio::fs::read(path).await.ok()?;
    AttemptResult::decode(bytes.as_slice()).ok()
}

fn stopped_attempt_result() -> AttemptResult {
    AttemptResult {
        launched: false,
        exit_code: None,
        error: String::new(),
        started_unix_ms: 0,
        ended_unix_ms: 0,
        stop_requested: true,
        forced: false,
        outcome: String::new(),
    }
}

fn supervisor_retry_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(31);
    Duration::from_millis(100_u64.saturating_mul(1_u64 << exponent).min(5_000))
}

async fn find_attempt_result(root: &Path) -> Option<AttemptResult> {
    read_attempt_result(&root.join("result.pb")).await
}

fn workload_view(id: &str, record: &WorkloadRecord, active: Option<&ActiveExecution>) -> Workload {
    Workload {
        workload_id: id.to_owned(),
        kind: record.kind.clone(),
        enabled: record.enabled,
        state: active.map_or_else(
            || {
                if record.definition.is_some() {
                    "inactive".to_owned()
                } else {
                    "definition-missing".to_owned()
                }
            },
            |active| active.state.to_owned(),
        ),
        execution_id: active.map_or_else(String::new, |active| active.execution_id.clone()),
        supervisor_process_id: active.map_or(0, |active| active.supervisor_process_id),
        workload_process_id: active.map_or(0, |active| active.workload_process_id),
        attempt: active.map_or(0, |active| active.attempt),
        error: active.map_or_else(String::new, |active| active.error.clone()),
        last_outcome: record.last_outcome.clone(),
        definition_missing: record.definition.is_none(),
        restart_required: active.is_some_and(|active| active.restart_required),
        policy_sync_pending: active.is_some_and(|active| active.policy_sync_pending),
    }
}

struct ActiveProgress {
    state: &'static str,
    workload_process_id: u32,
    attempt: u32,
    error: Option<String>,
}

fn active_state(state: &str) -> &'static str {
    match state {
        "starting" => "starting",
        "launching" => "launching",
        "running" => "running",
        "launch-failed" => "launch-failed",
        "attempt-exited" => "attempt-exited",
        "supervisor-lost" => "supervisor-lost",
        "restart-backoff" => "restart-backoff",
        "stopping" => "stopping",
        _ => "recovering",
    }
}

fn progress_observation(
    kind: &str,
    attempt: u32,
    workload_process_id: u32,
    detail: &str,
) -> Option<ActiveProgress> {
    let (state, workload_process_id, error) = match kind {
        "attempt-started" => ("running", workload_process_id, Some(String::new())),
        "launch-failed" => ("launch-failed", 0, Some(detail.to_owned())),
        "attempt-exited" => (
            "attempt-exited",
            0,
            (!detail.is_empty()).then(|| detail.to_owned()),
        ),
        "supervisor-lost" => ("supervisor-lost", 0, Some(detail.to_owned())),
        "restart-backoff" => (
            "restart-backoff",
            0,
            (!detail.is_empty()).then(|| detail.to_owned()),
        ),
        _ => return None,
    };
    Some(ActiveProgress {
        state,
        workload_process_id,
        attempt,
        error,
    })
}

fn execution_view(value: StoredExecution) -> Execution {
    Execution {
        execution_id: value.id,
        workload_id: value.workload_id,
        state: value.state,
        supervisor_process_id: value.supervisor_process_id,
        workload_process_id: value.workload_process_id,
        attempt: value.attempt,
        started_unix_ms: value.started_unix_ms,
        ended_unix_ms: value.ended_unix_ms,
        exit_code: value.exit_code,
        error: value.error,
    }
}

fn ensure_kind(
    id: &str,
    record: &WorkloadRecord,
    expected: &'static str,
) -> Result<(), ManagerError> {
    if record.kind == expected {
        Ok(())
    } else {
        Err(ManagerError::WrongKind {
            id: id.to_owned(),
            expected,
            actual: record.kind.clone(),
        })
    }
}

fn unix_ms() -> Result<i64, ManagerError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ManagerError::InvalidSystemTime)?
        .as_millis();
    i64::try_from(millis).map_err(|_| ManagerError::InvalidSystemTime)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn io_finish(
    state: &str,
    supervisor_process_id: u32,
    path: &Path,
    error: std::io::Error,
) -> ExecutionFinish {
    ExecutionFinish {
        state: state.to_owned(),
        exit_code: None,
        error: format!("{}: {error}", path.display()),
        supervisor_process_id,
    }
}
