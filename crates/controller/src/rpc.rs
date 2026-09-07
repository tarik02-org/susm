use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

use susm_protocol::{
    control::{
        CancelRequest, CancelResponse, DisableRequest, DisableResponse, EnableRequest,
        EnableResponse, GetExecutionRequest, GetExecutionResponse, GetWorkloadRequest,
        GetWorkloadResponse, ListExecutionsRequest, ListExecutionsResponse, ListWorkloadsRequest,
        ListWorkloadsResponse, ReadLogsRequest, ReadLogsResponse, ReloadRequest, ReloadResponse,
        RerunRequest, RerunResponse, RestartRequest, RestartResponse, RunRequest, RunResponse,
        StartRequest, StartResponse, StopRequest, StopResponse, WatchWorkloadsRequest,
        WatchWorkloadsResponse, control_service_server::ControlService,
    },
    pipe::CallerIdentity,
};

use crate::{Manager, ManagerError};

#[derive(Clone)]
pub struct ControlRpc {
    manager: Manager,
}

impl ControlRpc {
    pub const fn new(manager: Manager) -> Self {
        Self { manager }
    }
}

type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl ControlService for ControlRpc {
    type WatchWorkloadsStream = ResponseStream<WatchWorkloadsResponse>;
    type ReadLogsStream = ResponseStream<ReadLogsResponse>;

    async fn reload(
        &self,
        request: Request<ReloadRequest>,
    ) -> Result<Response<ReloadResponse>, Status> {
        authenticate(&request)?;
        match self.manager.reload() {
            Ok((changed, generation)) => Ok(Response::new(ReloadResponse {
                changed,
                generation,
                diagnostics: Vec::new(),
            })),
            Err(ManagerError::Config(error)) => Ok(Response::new(ReloadResponse {
                changed: false,
                generation: String::new(),
                diagnostics: vec![susm_protocol::control::Diagnostic {
                    path: self.manager.config_directory().display().to_string(),
                    message: error.to_string(),
                }],
            })),
            Err(error) => Err(map_error(error)),
        }
    }

    async fn list_workloads(
        &self,
        request: Request<ListWorkloadsRequest>,
    ) -> Result<Response<ListWorkloadsResponse>, Status> {
        authenticate(&request)?;
        let request = request.into_inner();
        let page_size = page_size(request.page_size)?;
        let workloads = self.manager.list_workloads();
        let start = if request.cursor.is_empty() {
            0
        } else {
            workloads.partition_point(|workload| workload.workload_id <= request.cursor)
        };
        let end = start
            .saturating_add(page_size as usize)
            .min(workloads.len());
        let next_cursor = if end < workloads.len() {
            workloads[end - 1].workload_id.clone()
        } else {
            String::new()
        };
        Ok(Response::new(ListWorkloadsResponse {
            workloads: workloads[start..end].to_vec(),
            next_cursor,
        }))
    }

    async fn get_workload(
        &self,
        request: Request<GetWorkloadRequest>,
    ) -> Result<Response<GetWorkloadResponse>, Status> {
        authenticate(&request)?;
        let id = workload_id(request.into_inner().workload_id)?;
        Ok(Response::new(GetWorkloadResponse {
            workload: Some(self.manager.workload(&id).map_err(map_error)?),
        }))
    }

    async fn start(
        &self,
        request: Request<StartRequest>,
    ) -> Result<Response<StartResponse>, Status> {
        authenticate(&request)?;
        let id = workload_id(request.into_inner().workload_id)?;
        let (changed, workload) = self.manager.start_service(&id).map_err(map_error)?;
        Ok(Response::new(StartResponse {
            changed,
            workload: Some(workload),
        }))
    }

    async fn stop(&self, request: Request<StopRequest>) -> Result<Response<StopResponse>, Status> {
        authenticate(&request)?;
        let id = workload_id(request.into_inner().workload_id)?;
        let (changed, workload) = self.manager.stop_service(&id).map_err(map_error)?;
        Ok(Response::new(StopResponse {
            changed,
            workload: Some(workload),
        }))
    }

    async fn restart(
        &self,
        request: Request<RestartRequest>,
    ) -> Result<Response<RestartResponse>, Status> {
        authenticate(&request)?;
        let id = workload_id(request.into_inner().workload_id)?;
        let (changed, workload) = self.manager.restart_service(&id).map_err(map_error)?;
        Ok(Response::new(RestartResponse {
            changed,
            workload: Some(workload),
        }))
    }

    async fn run(&self, request: Request<RunRequest>) -> Result<Response<RunResponse>, Status> {
        authenticate(&request)?;
        let id = workload_id(request.into_inner().workload_id)?;
        let (changed, workload) = self.manager.run_job(&id).map_err(map_error)?;
        Ok(Response::new(RunResponse {
            changed,
            workload: Some(workload),
        }))
    }

    async fn cancel(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        authenticate(&request)?;
        let id = workload_id(request.into_inner().workload_id)?;
        let (changed, workload) = self.manager.cancel_job(&id).map_err(map_error)?;
        Ok(Response::new(CancelResponse {
            changed,
            workload: Some(workload),
        }))
    }

    async fn rerun(
        &self,
        request: Request<RerunRequest>,
    ) -> Result<Response<RerunResponse>, Status> {
        authenticate(&request)?;
        let id = workload_id(request.into_inner().workload_id)?;
        let (changed, workload) = self.manager.rerun_job(&id).map_err(map_error)?;
        Ok(Response::new(RerunResponse {
            changed,
            workload: Some(workload),
        }))
    }

    async fn enable(
        &self,
        request: Request<EnableRequest>,
    ) -> Result<Response<EnableResponse>, Status> {
        authenticate(&request)?;
        let id = workload_id(request.into_inner().workload_id)?;
        let (changed, workload) = self.manager.set_enabled(&id, true).map_err(map_error)?;
        Ok(Response::new(EnableResponse {
            changed,
            workload: Some(workload),
        }))
    }

    async fn disable(
        &self,
        request: Request<DisableRequest>,
    ) -> Result<Response<DisableResponse>, Status> {
        authenticate(&request)?;
        let id = workload_id(request.into_inner().workload_id)?;
        let (changed, workload) = self.manager.set_enabled(&id, false).map_err(map_error)?;
        Ok(Response::new(DisableResponse {
            changed,
            workload: Some(workload),
        }))
    }

    async fn list_executions(
        &self,
        request: Request<ListExecutionsRequest>,
    ) -> Result<Response<ListExecutionsResponse>, Status> {
        authenticate(&request)?;
        let request = request.into_inner();
        let id = workload_id(request.workload_id)?;
        let before = if request.cursor.is_empty() {
            None
        } else {
            Some(
                request
                    .cursor
                    .parse::<i64>()
                    .map_err(|_| Status::invalid_argument("execution cursor is invalid"))?,
            )
        };
        let page_size = page_size(request.page_size)?;
        let executions = self
            .manager
            .list_executions(&id, page_size, before)
            .map_err(map_error)?;
        let next_cursor = executions
            .last()
            .filter(|_| executions.len() == page_size as usize)
            .map_or_else(String::new, |execution| {
                execution.started_unix_ms.to_string()
            });
        Ok(Response::new(ListExecutionsResponse {
            executions,
            next_cursor,
        }))
    }

    async fn get_execution(
        &self,
        request: Request<GetExecutionRequest>,
    ) -> Result<Response<GetExecutionResponse>, Status> {
        authenticate(&request)?;
        let id = request.into_inner().execution_id;
        uuid::Uuid::parse_str(&id)
            .map_err(|_| Status::invalid_argument("execution_id must be a UUID"))?;
        Ok(Response::new(GetExecutionResponse {
            execution: Some(self.manager.execution(&id).map_err(map_error)?),
        }))
    }

    async fn watch_workloads(
        &self,
        request: Request<WatchWorkloadsRequest>,
    ) -> Result<Response<Self::WatchWorkloadsStream>, Status> {
        authenticate(&request)?;
        let manager = self.manager.clone();
        let mut changes = manager.subscribe();
        let (sender, receiver) = mpsc::channel(16);
        tokio::spawn(async move {
            if sender.send(Ok(manager.snapshot())).await.is_err() {
                return;
            }
            loop {
                if changes.changed().await.is_err() {
                    return;
                }
                if sender.try_send(Ok(manager.snapshot())).is_err() {
                    let _ = sender
                        .send(Err(Status::resource_exhausted(
                            "workload watcher fell behind; resubscribe",
                        )))
                        .await;
                    return;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn read_logs(
        &self,
        request: Request<ReadLogsRequest>,
    ) -> Result<Response<Self::ReadLogsStream>, Status> {
        authenticate(&request)?;
        let request = request.into_inner();
        let workload = workload_id(request.workload_id.clone())?;
        self.manager.workload(&workload).map_err(map_error)?;
        let root = self.manager.data_directory().join("logs").join(workload);
        let (sender, receiver) = mpsc::channel(16);
        tokio::spawn(async move {
            if let Err(error) = stream_log_files(root, request, &sender).await {
                let _ = sender.send(Err(Status::internal(error.to_string()))).await;
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

fn authenticate<T>(request: &Request<T>) -> Result<(), Status> {
    request
        .extensions()
        .get::<CallerIdentity>()
        .ok_or_else(|| Status::unauthenticated("named-pipe caller identity is missing"))?;
    Ok(())
}

fn workload_id(value: String) -> Result<String, Status> {
    susm_domain::ids::WorkloadId::parse(value.clone())
        .map(|_| value)
        .map_err(|error| Status::invalid_argument(error.to_string()))
}

fn page_size(value: u32) -> Result<u32, Status> {
    match value {
        0 => Ok(100),
        1..=1000 => Ok(value),
        _ => Err(Status::invalid_argument("page_size must not exceed 1000")),
    }
}

fn map_error(error: ManagerError) -> Status {
    match error {
        ManagerError::NotFound(_) => Status::not_found(error.to_string()),
        ManagerError::AlreadyActive(_) => Status::already_exists(error.to_string()),
        ManagerError::DefinitionMissing(_)
        | ManagerError::WrongKind { .. }
        | ManagerError::KindChangeActive(_)
        | ManagerError::SupervisorIdentityMismatch
        | ManagerError::SessionEnding => Status::failed_precondition(error.to_string()),
        ManagerError::Config(_) => Status::invalid_argument(error.to_string()),
        ManagerError::Storage(_)
        | ManagerError::Encode(_)
        | ManagerError::Decode(_)
        | ManagerError::Io { .. }
        | ManagerError::InvalidSystemTime => Status::internal(error.to_string()),
    }
}

async fn stream_log_files(
    root: std::path::PathBuf,
    request: ReadLogsRequest,
    sender: &mpsc::Sender<Result<ReadLogsResponse, Status>>,
) -> std::io::Result<()> {
    let execution_filter = (!request.execution_id.is_empty()).then_some(request.execution_id);
    let mut seen = std::collections::BTreeSet::new();
    loop {
        if root.try_exists()? {
            let mut executions = tokio::fs::read_dir(&root).await?;
            while let Some(execution) = executions.next_entry().await? {
                let execution_id = execution.file_name().to_string_lossy().into_owned();
                if execution_filter
                    .as_deref()
                    .is_some_and(|filter| filter != execution_id)
                {
                    continue;
                }
                let mut attempts = tokio::fs::read_dir(execution.path()).await?;
                while let Some(attempt) = attempts.next_entry().await? {
                    let attempt_name = attempt.file_name().to_string_lossy().into_owned();
                    let attempt_number = attempt_name
                        .strip_prefix("attempt-")
                        .and_then(|value| value.parse::<u32>().ok())
                        .unwrap_or(0);
                    if request.attempt != 0 && request.attempt != attempt_number {
                        continue;
                    }
                    let mut files = tokio::fs::read_dir(attempt.path()).await?;
                    while let Some(file) = files.next_entry().await? {
                        let name = file.file_name().to_string_lossy().into_owned();
                        if !name.contains(".susm-journal") {
                            continue;
                        }
                        let path = file.path();
                        let bytes = tokio::fs::read(&path).await?;
                        let bytes = if path
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("zst"))
                        {
                            zstd::stream::decode_all(bytes.as_slice())?
                        } else {
                            bytes
                        };
                        for record in decode_journal(&bytes, &execution_id, attempt_number) {
                            if request.stream != "all" && request.stream != record.stream {
                                continue;
                            }
                            let identity = (
                                record.execution_id.clone(),
                                record.attempt,
                                record.sequence,
                                record.stream.clone(),
                            );
                            if seen.insert(identity) && sender.send(Ok(record)).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        if !request.follow {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if sender.is_closed() {
            return Ok(());
        }
    }
}

fn decode_journal(bytes: &[u8], execution_id: &str, attempt: u32) -> Vec<ReadLogsResponse> {
    let mut records = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let start = cursor;
        let mut stream = String::new();
        let mut sequence = 0;
        let mut timestamp = 0;
        let mut message = None;
        loop {
            let Some(line_end) = bytes[cursor..].iter().position(|byte| *byte == b'\n') else {
                cursor = bytes.len();
                break;
            };
            let line = &bytes[cursor..cursor + line_end];
            cursor += line_end + 1;
            if line.is_empty() {
                break;
            }
            if line == b"MESSAGE" {
                if cursor.saturating_add(8) > bytes.len() {
                    cursor = bytes.len();
                    break;
                }
                let length = u64::from_le_bytes(
                    bytes[cursor..cursor + 8]
                        .try_into()
                        .expect("slice length checked above"),
                );
                cursor += 8;
                let Ok(length) = usize::try_from(length) else {
                    cursor = bytes.len();
                    break;
                };
                if cursor.saturating_add(length).saturating_add(1) > bytes.len() {
                    cursor = bytes.len();
                    break;
                }
                message = Some(bytes[cursor..cursor + length].to_vec());
                cursor += length;
                if bytes[cursor] != b'\n' {
                    cursor = bytes.len();
                    break;
                }
                cursor += 1;
                continue;
            }
            if let Some(separator) = line.iter().position(|byte| *byte == b'=') {
                let name = &line[..separator];
                let value = &line[separator + 1..];
                let value = String::from_utf8_lossy(value);
                match name {
                    b"SUSM_STREAM" => stream = value.into_owned(),
                    b"SUSM_SEQUENCE" => sequence = value.parse().unwrap_or(0),
                    b"SUSM_TIMESTAMP_UNIX_MS" => timestamp = value.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        if cursor == start {
            break;
        }
        if let Some(message) = message {
            records.push(ReadLogsResponse {
                execution_id: execution_id.to_owned(),
                attempt,
                stream,
                sequence,
                timestamp_unix_ms: timestamp,
                message,
                gap: String::new(),
            });
        }
    }
    records
}
