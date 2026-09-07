use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use prost::Message;
use susm_protocol::{
    runtime::{Checkpoint, EnvironmentValue as RuntimeEnvironmentValue, RuntimeObservation},
    session::{EndingEvent, SessionEventError},
    supervisor::{AttemptResult, ExecutionConfiguration, PolicyUpdate},
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader},
    sync::{mpsc, watch},
    time::timeout,
};
use windows::Win32::System::{
    Console::{
        CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, GetConsoleProcessList, SetConsoleCtrlHandler,
    },
    Threading::CREATE_NO_WINDOW,
};
use windows::core::{BOOL, HRESULT};

use crate::{
    job::KillJob,
    journal::{
        OUTPUT_CHUNK_SIZE, OUTPUT_QUEUE_CHUNKS, OutputChunk, OutputStream, RuntimeJournal,
        run_writer,
    },
    process::{OutputMode, ProcessError, ProcessSpec, WindowsChild, fresh_environment},
};

const MAX_SPEC_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum RunError {
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("execution snapshot exceeds 4 MiB")]
    SpecTooLarge,
    #[error("invalid execution snapshot: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("preflight failed: {0}")]
    Preflight(String),
    #[error("Windows process isolation failed: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("process launch failed: {0}")]
    Process(#[from] ProcessError),
    #[error("manager-session ending event failed: {0}")]
    SessionEvent(#[from] SessionEventError),
}

#[derive(Clone, Debug)]
pub struct SupervisorIdentity {
    pub id: String,
    pub incarnation: u32,
}

pub async fn load_configuration(spec_path: &Path) -> Result<ExecutionConfiguration, RunError> {
    let metadata = tokio::fs::metadata(spec_path)
        .await
        .map_err(|source| io_error(spec_path, source))?;
    if metadata.len() > MAX_SPEC_BYTES {
        return Err(RunError::SpecTooLarge);
    }
    let bytes = tokio::fs::read(spec_path)
        .await
        .map_err(|source| io_error(spec_path, source))?;
    ExecutionConfiguration::decode(bytes.as_slice()).map_err(Into::into)
}

pub async fn run(
    mut configuration: ExecutionConfiguration,
    journal: RuntimeJournal,
    identity: SupervisorIdentity,
    mut external_stop: watch::Receiver<bool>,
    mut policy: watch::Receiver<Option<PolicyUpdate>>,
    observations: mpsc::UnboundedSender<RuntimeObservation>,
) -> Result<AttemptResult, RunError> {
    let recovery = journal.recovery();
    let mut recorder = RuntimeRecorder {
        journal,
        observations,
        identity,
        sequence: recovery.last_sequence,
        committed_sequence: recovery.committed_sequence,
    };
    let ending_event = if configuration.manager_session_id.is_empty() {
        None
    } else {
        Some(EndingEvent::open(&configuration.manager_session_id)?)
    };
    let mut preflight = recovery.checkpoint.as_ref().and_then(recovered_preflight);
    let mut attempt = recovery
        .checkpoint
        .as_ref()
        .map_or(configuration.attempt.max(1), |checkpoint| {
            checkpoint.attempt.max(1)
        });
    let mut retries = attempt.saturating_sub(1);
    let output_sequence = Arc::new(AtomicU64::new(0));
    let (stdin_stop, mut stdin_stop_rx) = mpsc::channel(1);
    tokio::spawn(watch_stdin(stdin_stop));
    ensure_private_console()?;

    if let Some(checkpoint) = &recovery.checkpoint
        && matches!(checkpoint.phase.as_str(), "launching" | "running")
    {
        if stop_requested(&external_stop, ending_event.as_ref()) {
            let result = stopped_result(unix_ms());
            finish_execution(&configuration, &mut recorder, attempt, &result).await?;
            return Ok(result);
        }
        let mut result = AttemptResult {
            launched: checkpoint.phase == "running",
            exit_code: None,
            error: "previous supervisor disappeared before persisting the process outcome"
                .to_owned(),
            started_unix_ms: 0,
            ended_unix_ms: unix_ms(),
            stop_requested: false,
            forced: true,
            outcome: String::new(),
        };
        write_attempt_result(&configuration, attempt, &result).await?;
        recorder.observe(ObservationFacts {
            kind: "supervisor-lost",
            attempt,
            exit_code: None,
            detail: result.error.clone(),
            terminal: false,
            workload_process_id: 0,
        })?;
        if configuration.kind == "job" {
            result.outcome = "outcome-unknown".to_owned();
            finish_execution_kind(
                &configuration,
                &mut recorder,
                attempt,
                &result,
                "outcome-unknown",
            )
            .await?;
            return Ok(result);
        }
        if !should_retry(&configuration, false, retries) {
            finish_execution(&configuration, &mut recorder, attempt, &result).await?;
            return Ok(result);
        }
        retries = retries.saturating_add(1);
        attempt = attempt.saturating_add(1);
        let delay = retry_delay(&configuration, retries);
        let retry_at =
            unix_ms().saturating_add(i64::try_from(delay.as_millis()).unwrap_or(i64::MAX));
        recorder.checkpoint(
            &configuration,
            attempt,
            "restart_backoff",
            retry_at,
            preflight.as_ref(),
        )?;
        recorder.observe(ObservationFacts {
            kind: "restart-backoff",
            attempt,
            exit_code: None,
            detail: "recovering from supervisor loss".to_owned(),
            terminal: false,
            workload_process_id: 0,
        })?;
        match wait_backoff(
            delay,
            BackoffContext {
                configuration: &mut configuration,
                recorder: &mut recorder,
                attempt,
                retry_at_unix_ms: retry_at,
                preflight: preflight.as_ref(),
                successful: false,
                decision_retries: retries.saturating_sub(1),
                external_stop: &mut external_stop,
                stdin_stop: &mut stdin_stop_rx,
                ending_event: ending_event.as_ref(),
                policy: &mut policy,
            },
        )
        .await?
        {
            BackoffOutcome::Elapsed => {}
            BackoffOutcome::Stopped => {
                let stopped = stopped_result(unix_ms());
                finish_execution(&configuration, &mut recorder, attempt, &stopped).await?;
                return Ok(stopped);
            }
            BackoffOutcome::RetryDenied => {
                finish_execution(&configuration, &mut recorder, attempt, &result).await?;
                return Ok(result);
            }
        }
    }

    if let Some(checkpoint) = &recovery.checkpoint
        && checkpoint.phase == "restart_backoff"
        && checkpoint.retry_at_unix_ms > unix_ms()
    {
        let remaining = checkpoint.retry_at_unix_ms.saturating_sub(unix_ms());
        match wait_backoff(
            Duration::from_millis(u64::try_from(remaining).unwrap_or(u64::MAX)),
            BackoffContext {
                configuration: &mut configuration,
                recorder: &mut recorder,
                attempt,
                retry_at_unix_ms: checkpoint.retry_at_unix_ms,
                preflight: preflight.as_ref(),
                successful: false,
                decision_retries: attempt.saturating_sub(2),
                external_stop: &mut external_stop,
                stdin_stop: &mut stdin_stop_rx,
                ending_event: ending_event.as_ref(),
                policy: &mut policy,
            },
        )
        .await?
        {
            BackoffOutcome::Elapsed => {}
            BackoffOutcome::Stopped => {
                let stopped = stopped_result(unix_ms());
                finish_execution(&configuration, &mut recorder, attempt, &stopped).await?;
                return Ok(stopped);
            }
            BackoffOutcome::RetryDenied => {
                let failed = failed_result(
                    unix_ms(),
                    "pending retry was denied by a policy update".to_owned(),
                );
                finish_execution(&configuration, &mut recorder, attempt, &failed).await?;
                return Ok(failed);
            }
        }
    }

    loop {
        configuration.attempt = attempt;
        recorder.checkpoint(&configuration, attempt, "launching", 0, preflight.as_ref())?;
        let started_unix_ms = unix_ms();
        if policy.has_changed().unwrap_or(false)
            && let Some(update) = policy.borrow_and_update().clone()
        {
            apply_policy(&mut configuration, &update);
            recorder.checkpoint(&configuration, attempt, "launching", 0, preflight.as_ref())?;
            recorder.observe(ObservationFacts {
                kind: "policy-applied",
                attempt,
                exit_code: None,
                detail: hex(&update.policy_hash),
                terminal: false,
                workload_process_id: 0,
            })?;
        }
        let result = match preflight.as_ref() {
            Some(preflight) => {
                run_process(
                    &mut configuration,
                    attempt,
                    preflight.clone(),
                    started_unix_ms,
                    StopSources {
                        external: &mut external_stop,
                        stdin: &mut stdin_stop_rx,
                        ending_event: ending_event.as_ref(),
                        policy: &mut policy,
                    },
                    output_sequence.clone(),
                    &mut recorder,
                )
                .await
            }
            None => match preflight_configuration(&configuration) {
                Ok(resolved) => {
                    recorder.checkpoint(
                        &configuration,
                        attempt,
                        "launching",
                        0,
                        Some(&resolved),
                    )?;
                    let result = run_process(
                        &mut configuration,
                        attempt,
                        resolved.clone(),
                        started_unix_ms,
                        StopSources {
                            external: &mut external_stop,
                            stdin: &mut stdin_stop_rx,
                            ending_event: ending_event.as_ref(),
                            policy: &mut policy,
                        },
                        output_sequence.clone(),
                        &mut recorder,
                    )
                    .await;
                    preflight = Some(resolved);
                    result
                }
                Err(error) => AttemptResult {
                    launched: false,
                    exit_code: None,
                    error: error.to_string(),
                    started_unix_ms,
                    ended_unix_ms: unix_ms(),
                    stop_requested: stop_requested(&external_stop, ending_event.as_ref()),
                    forced: false,
                    outcome: String::new(),
                },
            },
        };
        write_attempt_result(&configuration, attempt, &result).await?;
        recorder.observe(ObservationFacts {
            kind: if result.launched {
                "attempt-exited"
            } else {
                "launch-failed"
            },
            attempt,
            exit_code: result.exit_code,
            detail: result.error.clone(),
            terminal: false,
            workload_process_id: 0,
        })?;

        if result.stop_requested {
            finish_execution(&configuration, &mut recorder, attempt, &result).await?;
            return Ok(result);
        }

        let successful = result
            .exit_code
            .is_some_and(|code| configuration.success_exit_codes.contains(&code));
        if result.launched
            && configuration.restart_reset_after_ms != 0
            && result.ended_unix_ms.saturating_sub(result.started_unix_ms)
                >= i64::try_from(configuration.restart_reset_after_ms).unwrap_or(i64::MAX)
        {
            retries = 0;
        }
        if !should_retry(&configuration, successful, retries) {
            finish_execution(&configuration, &mut recorder, attempt, &result).await?;
            return Ok(result);
        }

        retries = retries.saturating_add(1);
        attempt = attempt.saturating_add(1);
        let delay = retry_delay(&configuration, retries);
        let retry_at =
            unix_ms().saturating_add(i64::try_from(delay.as_millis()).unwrap_or(i64::MAX));
        recorder.checkpoint(
            &configuration,
            attempt,
            "restart_backoff",
            retry_at,
            preflight.as_ref(),
        )?;
        recorder.observe(ObservationFacts {
            kind: "restart-backoff",
            attempt,
            exit_code: result.exit_code,
            detail: result.error.clone(),
            terminal: false,
            workload_process_id: 0,
        })?;
        match wait_backoff(
            delay,
            BackoffContext {
                configuration: &mut configuration,
                recorder: &mut recorder,
                attempt,
                retry_at_unix_ms: retry_at,
                preflight: preflight.as_ref(),
                successful,
                decision_retries: retries.saturating_sub(1),
                external_stop: &mut external_stop,
                stdin_stop: &mut stdin_stop_rx,
                ending_event: ending_event.as_ref(),
                policy: &mut policy,
            },
        )
        .await?
        {
            BackoffOutcome::Elapsed => {}
            BackoffOutcome::Stopped => {
                let stopped = stopped_result(unix_ms());
                finish_execution(&configuration, &mut recorder, attempt, &stopped).await?;
                return Ok(stopped);
            }
            BackoffOutcome::RetryDenied => {
                finish_execution(&configuration, &mut recorder, attempt, &result).await?;
                return Ok(result);
            }
        }
    }
}

struct RuntimeRecorder {
    journal: RuntimeJournal,
    observations: mpsc::UnboundedSender<RuntimeObservation>,
    identity: SupervisorIdentity,
    sequence: u64,
    committed_sequence: u64,
}

struct StopSources<'a> {
    external: &'a mut watch::Receiver<bool>,
    stdin: &'a mut mpsc::Receiver<()>,
    ending_event: Option<&'a EndingEvent>,
    policy: &'a mut watch::Receiver<Option<PolicyUpdate>>,
}

struct BackoffContext<'a> {
    configuration: &'a mut ExecutionConfiguration,
    recorder: &'a mut RuntimeRecorder,
    attempt: u32,
    retry_at_unix_ms: i64,
    preflight: Option<&'a Preflight>,
    successful: bool,
    decision_retries: u32,
    external_stop: &'a mut watch::Receiver<bool>,
    stdin_stop: &'a mut mpsc::Receiver<()>,
    ending_event: Option<&'a EndingEvent>,
    policy: &'a mut watch::Receiver<Option<PolicyUpdate>>,
}

enum BackoffOutcome {
    Elapsed,
    Stopped,
    RetryDenied,
}

impl RuntimeRecorder {
    fn checkpoint(
        &self,
        configuration: &ExecutionConfiguration,
        attempt: u32,
        phase: &str,
        retry_at_unix_ms: i64,
        preflight: Option<&Preflight>,
    ) -> Result<(), RunError> {
        let (resolved_executable, resolved_working_directory, resolved_environment) = preflight
            .map(|preflight| {
                (
                    preflight.executable.to_string_lossy().into_owned(),
                    preflight.working_directory.to_string_lossy().into_owned(),
                    preflight
                        .environment
                        .iter()
                        .map(|(name, value)| RuntimeEnvironmentValue {
                            name: name.clone(),
                            value: value.clone(),
                        })
                        .collect(),
                )
            })
            .unwrap_or_default();
        self.journal
            .append_checkpoint(Checkpoint {
                manager_session_id: configuration.manager_session_id.clone(),
                workload_id: configuration.workload_id.clone(),
                execution_id: configuration.execution_id.clone(),
                supervisor_id: self.identity.id.clone(),
                incarnation: self.identity.incarnation,
                execution_config_hash: configuration.execution_config_hash.clone(),
                attempt,
                phase: phase.to_owned(),
                last_sequence: self.sequence,
                committed_sequence: self.committed_sequence,
                retry_at_unix_ms,
                resolved_executable,
                resolved_working_directory,
                resolved_environment,
            })
            .map_err(|source| io_error(&self.journal.path(), source))
    }

    fn observe(&mut self, facts: ObservationFacts<'_>) -> Result<(), RunError> {
        self.sequence = self.sequence.saturating_add(1);
        let observation = RuntimeObservation {
            sequence: self.sequence,
            kind: facts.kind.to_owned(),
            attempt: facts.attempt,
            exit_code: facts.exit_code,
            detail: facts.detail,
            observed_unix_ms: unix_ms(),
            terminal: facts.terminal,
            workload_process_id: facts.workload_process_id,
        };
        self.journal
            .append_observation(observation.clone())
            .map_err(|source| io_error(&self.journal.path(), source))?;
        tracing::info!(
            name = "supervisor_observation",
            supervisor_id = %self.identity.id,
            incarnation = self.identity.incarnation,
            sequence = observation.sequence,
            kind = %observation.kind,
            attempt = observation.attempt,
            exit_code = ?observation.exit_code,
            terminal = observation.terminal,
            workload_process_id = observation.workload_process_id,
            detail = %observation.detail,
        );
        let _ = self.observations.send(observation);
        Ok(())
    }
}

struct ObservationFacts<'a> {
    kind: &'a str,
    attempt: u32,
    exit_code: Option<u32>,
    detail: String,
    terminal: bool,
    workload_process_id: u32,
}

#[derive(Clone)]
struct Preflight {
    executable: PathBuf,
    working_directory: PathBuf,
    environment: BTreeMap<String, String>,
}

fn preflight_configuration(configuration: &ExecutionConfiguration) -> Result<Preflight, RunError> {
    let base = fresh_environment()?;
    let working_directory = PathBuf::from(expand(&configuration.working_directory, &base)?);
    if !working_directory.is_absolute() || !working_directory.is_dir() {
        return Err(RunError::Preflight(format!(
            "working directory is not an existing absolute directory: {}",
            working_directory.display()
        )));
    }
    let executable_value = expand(&configuration.executable, &base)?;
    let executable = resolve_executable(&executable_value, &working_directory, &base)?;
    let mut environment = base.clone();
    for value in &configuration.environment_set {
        environment.insert(value.name.to_uppercase(), expand(&value.value, &base)?);
    }
    for name in &configuration.environment_unset {
        environment.remove(&name.to_uppercase());
    }
    Ok(Preflight {
        executable,
        working_directory,
        environment,
    })
}

fn recovered_preflight(checkpoint: &Checkpoint) -> Option<Preflight> {
    let executable = PathBuf::from(&checkpoint.resolved_executable);
    let working_directory = PathBuf::from(&checkpoint.resolved_working_directory);
    if !executable.is_absolute() || !working_directory.is_absolute() {
        return None;
    }
    let environment = checkpoint
        .resolved_environment
        .iter()
        .map(|value| (value.name.clone(), value.value.clone()))
        .collect();
    Some(Preflight {
        executable,
        working_directory,
        environment,
    })
}

async fn run_process(
    configuration: &mut ExecutionConfiguration,
    attempt: u32,
    preflight: Preflight,
    started_unix_ms: i64,
    stop: StopSources<'_>,
    output_sequence: Arc<AtomicU64>,
    recorder: &mut RuntimeRecorder,
) -> AttemptResult {
    let attempt_configuration = attempt_configuration(configuration, attempt);
    let job = match KillJob::create() {
        Ok(job) => job,
        Err(error) => {
            return failed_result(started_unix_ms, format!("Job Object setup failed: {error}"));
        }
    };
    let mut child = match WindowsChild::spawn(
        ProcessSpec {
            executable: &preflight.executable,
            arguments: &attempt_configuration.arguments,
            working_directory: &preflight.working_directory,
            environment: &preflight.environment,
            output: OutputMode::Piped,
            extra_creation_flags: Default::default(),
        },
        &job,
    ) {
        Ok(child) => child,
        Err(error) => return failed_result(started_unix_ms, error.to_string()),
    };
    if let Err(error) = recorder.checkpoint(configuration, attempt, "running", 0, Some(&preflight))
    {
        let _ = job.terminate();
        return failed_result(
            started_unix_ms,
            format!("failed to persist running state: {error}"),
        );
    }
    let process_id = child.id();
    if let Err(error) = recorder.observe(ObservationFacts {
        kind: "attempt-started",
        attempt,
        exit_code: None,
        detail: String::new(),
        terminal: false,
        workload_process_id: process_id,
    }) {
        let _ = job.terminate();
        return failed_result(
            started_unix_ms,
            format!("failed to persist attempt start: {error}"),
        );
    }

    let (chunks, receiver) = mpsc::channel(OUTPUT_QUEUE_CHUNKS);
    let stdout_task = child.take_stdout().map(|stdout| {
        tokio::spawn(drain_output(
            stdout,
            OutputStream::Stdout,
            chunks.clone(),
            output_sequence.clone(),
        ))
    });
    let stderr_task = child.take_stderr().map(|stderr| {
        tokio::spawn(drain_output(
            stderr,
            OutputStream::Stderr,
            chunks.clone(),
            output_sequence,
        ))
    });
    drop(chunks);
    let writer_task = tokio::spawn(run_writer(
        attempt_configuration,
        receiver,
        stop.policy.clone(),
    ));

    let mut policy_open = true;
    let (status, stop_requested, forced) = loop {
        tokio::select! {
            biased;
            result = wait_for_manager_session(stop.ending_event) => {
                let _ = result;
                break stop_process(
                    configuration,
                    &preflight,
                    &child,
                    &job,
                    process_id,
                    Some(Duration::from_secs(25)),
                ).await;
            }
            status = child.wait() => break (status, false, false),
            changed = stop.external.changed() => {
                if changed.is_ok() && *stop.external.borrow() {
                    break stop_process(
                        configuration,
                        &preflight,
                        &child,
                        &job,
                        process_id,
                        session_deadline(stop.ending_event),
                    ).await;
                } else if changed.is_err() {
                    break (child.wait().await, false, false);
                }
            }
            signal = stop.stdin.recv() => {
                if signal.is_some() {
                    break stop_process(
                        configuration,
                        &preflight,
                        &child,
                        &job,
                        process_id,
                        session_deadline(stop.ending_event),
                    ).await;
                } else {
                    break (child.wait().await, false, false);
                }
            }
            changed = stop.policy.changed(), if policy_open => {
                if changed.is_err() {
                    policy_open = false;
                    continue;
                }
                if let Some(update) = stop.policy.borrow_and_update().clone() {
                    let mut candidate = configuration.clone();
                    apply_policy(&mut candidate, &update);
                    if recorder
                        .checkpoint(&candidate, attempt, "running", 0, Some(&preflight))
                        .is_ok()
                    {
                        *configuration = candidate;
                        let _ = recorder.observe(ObservationFacts {
                            kind: "policy-applied",
                            attempt,
                            exit_code: None,
                            detail: hex(&update.policy_hash),
                            terminal: false,
                            workload_process_id: process_id,
                        });
                    }
                }
            }
        }
    };
    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }
    let _ = writer_task.await;
    match status {
        Ok(status) => AttemptResult {
            launched: true,
            exit_code: Some(status),
            error: String::new(),
            started_unix_ms,
            ended_unix_ms: unix_ms(),
            stop_requested,
            forced,
            outcome: String::new(),
        },
        Err(error) => failed_result(started_unix_ms, format!("process wait failed: {error}")),
    }
}

fn session_deadline(ending_event: Option<&EndingEvent>) -> Option<Duration> {
    ending_event
        .and_then(|event| event.is_signaled().ok())
        .filter(|signaled| *signaled)
        .map(|_| Duration::from_secs(25))
}

fn ensure_private_console() -> windows::core::Result<()> {
    let options = AllocConsoleOptions {
        mode: ALLOC_CONSOLE_MODE_NO_WINDOW,
        use_show_window: BOOL(0),
        show_window: 0,
    };
    let mut result = 0;
    unsafe {
        AllocConsoleWithOptions(&options, &mut result).ok()?;
    }
    if result == ALLOC_CONSOLE_RESULT_NO_CONSOLE {
        return Err(windows::core::Error::new(
            HRESULT(0x8000_4005_u32 as i32),
            "Windows did not allocate a private console",
        ));
    }
    let mut attached_process = [0];
    if unsafe { GetConsoleProcessList(&mut attached_process) } == 0 {
        return Err(windows::core::Error::from_thread());
    }
    unsafe {
        SetConsoleCtrlHandler(None, true)?;
    }
    Ok(())
}

const ALLOC_CONSOLE_MODE_NO_WINDOW: i32 = 2;
const ALLOC_CONSOLE_RESULT_NO_CONSOLE: i32 = 0;

#[repr(C)]
struct AllocConsoleOptions {
    mode: i32,
    use_show_window: BOOL,
    show_window: u16,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AllocConsoleWithOptions(options: *const AllocConsoleOptions, result: *mut i32) -> HRESULT;
}

async fn stop_process(
    configuration: &ExecutionConfiguration,
    preflight: &Preflight,
    child: &WindowsChild,
    job: &KillJob,
    process_id: u32,
    maximum_timeout: Option<Duration>,
) -> (Result<u32, ProcessError>, bool, bool) {
    if configuration.stop_method == "kill" {
        let _ = job.terminate();
        return (child.wait().await, true, true);
    }
    let stop_command = if configuration.stop_method == "command" {
        start_stop_command(configuration, preflight)
    } else {
        unsafe {
            let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_id);
        }
        None
    };
    let configured = Duration::from_millis(configuration.stop_timeout_ms);
    let wait = maximum_timeout.map_or(configured, |maximum| configured.min(maximum));
    let outcome = match timeout(wait, child.wait()).await {
        Ok(status) => (status, true, false),
        Err(_) => {
            let _ = job.terminate();
            (child.wait().await, true, true)
        }
    };
    if let Some((command, command_job)) = stop_command {
        let _ = command_job.terminate();
        let _ = command.wait().await;
    }
    outcome
}

fn start_stop_command(
    configuration: &ExecutionConfiguration,
    preflight: &Preflight,
) -> Option<(WindowsChild, KillJob)> {
    let executable = resolve_executable(
        &configuration.stop_executable,
        &preflight.working_directory,
        &preflight.environment,
    )
    .ok()?;
    let job = KillJob::create().ok()?;
    let child = WindowsChild::spawn(
        ProcessSpec {
            executable: &executable,
            arguments: &configuration.stop_arguments,
            working_directory: &preflight.working_directory,
            environment: &preflight.environment,
            output: OutputMode::Discard,
            extra_creation_flags: CREATE_NO_WINDOW,
        },
        &job,
    )
    .ok()?;
    Some((child, job))
}

async fn drain_output(
    mut reader: impl AsyncRead + Unpin,
    stream: OutputStream,
    sender: mpsc::Sender<OutputChunk>,
    sequence: Arc<AtomicU64>,
) {
    let mut buffer = vec![0; OUTPUT_CHUNK_SIZE];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        let chunk = OutputChunk {
            stream,
            sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
            timestamp_unix_ms: unix_ms(),
            bytes: buffer[..read].to_vec(),
        };
        if sender.try_send(chunk).is_err() {
            continue;
        }
    }
}

async fn watch_stdin(sender: mpsc::Sender<()>) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().eq_ignore_ascii_case("stop") {
            let _ = sender.send(()).await;
            return;
        }
    }
    std::future::pending::<()>().await;
}

async fn wait_backoff(
    delay: Duration,
    context: BackoffContext<'_>,
) -> Result<BackoffOutcome, RunError> {
    let deadline = tokio::time::Instant::now() + delay;
    let mut stdin_open = true;
    let mut policy_open = true;
    loop {
        tokio::select! {
            biased;
            _ = wait_for_manager_session(context.ending_event) => {
                return Ok(BackoffOutcome::Stopped);
            }
            changed = context.external_stop.changed() => {
                if changed.is_ok() && *context.external_stop.borrow() {
                    return Ok(BackoffOutcome::Stopped);
                }
            }
            signal = context.stdin_stop.recv(), if stdin_open => {
                if signal.is_some() {
                    return Ok(BackoffOutcome::Stopped);
                }
                stdin_open = false;
            }
            changed = context.policy.changed(), if policy_open => {
                if changed.is_err() {
                    policy_open = false;
                    continue;
                }
                let Some(update) = context.policy.borrow_and_update().clone() else {
                    continue;
                };
                let mut candidate = context.configuration.clone();
                apply_policy(&mut candidate, &update);
                context.recorder.checkpoint(
                    &candidate,
                    context.attempt,
                    "restart_backoff",
                    context.retry_at_unix_ms,
                    context.preflight,
                )?;
                *context.configuration = candidate;
                context.recorder.observe(ObservationFacts {
                    kind: "policy-applied",
                    attempt: context.attempt,
                    exit_code: None,
                    detail: hex(&update.policy_hash),
                    terminal: false,
                    workload_process_id: 0,
                })?;
                if !should_retry(
                    context.configuration,
                    context.successful,
                    context.decision_retries,
                ) {
                    return Ok(BackoffOutcome::RetryDenied);
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(BackoffOutcome::Elapsed);
            }
        }
    }
}

async fn wait_for_manager_session(event: Option<&EndingEvent>) -> Result<(), SessionEventError> {
    match event {
        Some(event) => event.wait().await,
        None => std::future::pending().await,
    }
}

fn stop_requested(
    external_stop: &watch::Receiver<bool>,
    ending_event: Option<&EndingEvent>,
) -> bool {
    *external_stop.borrow()
        || ending_event
            .and_then(|event| event.is_signaled().ok())
            .unwrap_or(false)
}

fn should_retry(configuration: &ExecutionConfiguration, successful: bool, retries: u32) -> bool {
    let policy_allows = configuration.restart_policy == "always"
        || (configuration.restart_policy == "on-failure" && !successful);
    policy_allows && (configuration.max_restarts_unlimited || retries < configuration.max_restarts)
}

fn retry_delay(configuration: &ExecutionConfiguration, retry: u32) -> Duration {
    let multiplier = u64::from(configuration.restart_backoff_multiplier.max(1));
    let maximum = configuration.restart_backoff_maximum_ms.max(1);
    let mut delay = configuration.restart_backoff_initial_ms.max(1);
    for _ in 1..retry {
        delay = delay.saturating_mul(multiplier).min(maximum);
    }
    Duration::from_millis(delay.min(maximum))
}

fn attempt_configuration(
    configuration: &ExecutionConfiguration,
    attempt: u32,
) -> ExecutionConfiguration {
    let mut attempt_configuration = configuration.clone();
    attempt_configuration.attempt = attempt;
    attempt_configuration.log_directory = Path::new(&configuration.log_directory)
        .join(format!("attempt-{attempt:06}"))
        .to_string_lossy()
        .into_owned();
    attempt_configuration
}

fn apply_policy(configuration: &mut ExecutionConfiguration, update: &PolicyUpdate) {
    configuration
        .success_exit_codes
        .clone_from(&update.success_exit_codes);
    configuration
        .restart_policy
        .clone_from(&update.restart_policy);
    configuration.max_restarts = update.max_restarts;
    configuration.max_restarts_unlimited = update.max_restarts_unlimited;
    configuration.restart_reset_after_ms = update.restart_reset_after_ms;
    configuration.restart_backoff_initial_ms = update.restart_backoff_initial_ms;
    configuration.restart_backoff_multiplier = update.restart_backoff_multiplier;
    configuration.restart_backoff_maximum_ms = update.restart_backoff_maximum_ms;
    configuration.stop_method.clone_from(&update.stop_method);
    configuration.stop_timeout_ms = update.stop_timeout_ms;
    configuration
        .stop_executable
        .clone_from(&update.stop_executable);
    configuration
        .stop_arguments
        .clone_from(&update.stop_arguments);
    configuration.capture_logs = update.capture_logs;
    configuration.segment_size = update.segment_size;
    configuration.segment_age_ms = update.segment_age_ms;
    configuration.retention_size = update.retention_size;
    configuration.retention_size_unlimited = update.retention_size_unlimited;
    configuration.retention_age_ms = update.retention_age_ms;
    configuration.retention_age_unlimited = update.retention_age_unlimited;
}

async fn finish_execution(
    configuration: &ExecutionConfiguration,
    recorder: &mut RuntimeRecorder,
    attempt: u32,
    result: &AttemptResult,
) -> Result<(), RunError> {
    let successful = result
        .exit_code
        .is_some_and(|code| configuration.success_exit_codes.contains(&code));
    let kind = if result.stop_requested {
        if configuration.kind == "job" {
            "cancelled"
        } else {
            "stopped"
        }
    } else if successful {
        "completed"
    } else {
        "failed"
    };
    finish_execution_kind(configuration, recorder, attempt, result, kind).await
}

async fn finish_execution_kind(
    configuration: &ExecutionConfiguration,
    recorder: &mut RuntimeRecorder,
    attempt: u32,
    result: &AttemptResult,
    kind: &str,
) -> Result<(), RunError> {
    recorder.observe(ObservationFacts {
        kind,
        attempt,
        exit_code: result.exit_code,
        detail: result.error.clone(),
        terminal: true,
        workload_process_id: 0,
    })?;
    let mut terminal_result = result.clone();
    terminal_result.outcome = kind.to_owned();
    write_result(Path::new(&configuration.result_file), &terminal_result).await
}

async fn write_attempt_result(
    configuration: &ExecutionConfiguration,
    attempt: u32,
    result: &AttemptResult,
) -> Result<(), RunError> {
    let root = Path::new(&configuration.runtime_journal)
        .parent()
        .ok_or_else(|| RunError::Preflight("runtime journal has no parent".to_owned()))?;
    write_result(
        &root.join(format!("attempt-{attempt:06}")).join("result.pb"),
        result,
    )
    .await
}

async fn write_result(path: &Path, result: &AttemptResult) -> Result<(), RunError> {
    let parent = path
        .parent()
        .ok_or_else(|| RunError::Preflight("result path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|source| io_error(parent, source))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    tokio::fs::write(&temporary, result.encode_to_vec())
        .await
        .map_err(|source| io_error(&temporary, source))?;
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|source| io_error(path, source))
}

fn stopped_result(started_unix_ms: i64) -> AttemptResult {
    AttemptResult {
        launched: false,
        exit_code: None,
        error: String::new(),
        started_unix_ms,
        ended_unix_ms: unix_ms(),
        stop_requested: true,
        forced: false,
        outcome: String::new(),
    }
}

fn failed_result(started_unix_ms: i64, error: String) -> AttemptResult {
    AttemptResult {
        launched: false,
        exit_code: None,
        error,
        started_unix_ms,
        ended_unix_ms: unix_ms(),
        stop_requested: false,
        forced: false,
        outcome: String::new(),
    }
}

fn resolve_executable(
    value: &str,
    working_directory: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<PathBuf, RunError> {
    let path = PathBuf::from(value);
    if path.components().count() > 1 {
        if path.is_absolute() && path.is_file() {
            return Ok(path);
        }
        return Err(RunError::Preflight(format!(
            "executable is not an existing absolute file: {value}"
        )));
    }
    let path_value = environment
        .get("PATH")
        .ok_or_else(|| RunError::Preflight("PATH is missing".to_owned()))?;
    for directory in std::env::split_paths(path_value) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            working_directory.join(directory)
        };
        let exact = directory.join(&path);
        if exact.is_file() {
            return Ok(exact);
        }
        if path.extension().is_none() {
            let executable = directory.join(format!("{value}.exe"));
            if executable.is_file() {
                return Ok(executable);
            }
        }
    }
    Err(RunError::Preflight(format!(
        "executable was not found on PATH: {value}"
    )))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn expand(value: &str, environment: &BTreeMap<String, String>) -> Result<String, RunError> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(dollar) = rest.find('$') {
        output.push_str(&rest[..dollar]);
        rest = &rest[dollar + 1..];
        if let Some(after) = rest.strip_prefix('$') {
            output.push('$');
            rest = after;
        } else if let Some(after_open) = rest.strip_prefix('{') {
            let end = after_open.find('}').ok_or_else(|| {
                RunError::Preflight("unterminated environment reference".to_owned())
            })?;
            let name = &after_open[..end];
            let replacement = environment.get(&name.to_uppercase()).ok_or_else(|| {
                RunError::Preflight(format!("environment variable is missing: {name}"))
            })?;
            output.push_str(replacement);
            rest = &after_open[end + 1..];
        } else {
            return Err(RunError::Preflight(
                "a single '$' is not a valid interpolation".to_owned(),
            ));
        }
    }
    output.push_str(rest);
    Ok(output)
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn io_error(path: &Path, source: io::Error) -> RunError {
    RunError::Io {
        path: path.to_owned(),
        source,
    }
}
