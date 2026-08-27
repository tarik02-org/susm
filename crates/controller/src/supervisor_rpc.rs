use std::pin::Pin;

use susm_protocol::{
    pipe::CallerIdentity,
    supervisor::{
        Acknowledgement, AttachRequest, AttachResponse, Command as SupervisorCommand, Welcome,
        attach_request, supervisor_control_service_server::SupervisorControlService,
    },
};
use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, Streaming};

use crate::{Manager, ManagerError, manager::SupervisorObservation};

#[derive(Clone)]
pub struct SupervisorRpc {
    manager: Manager,
}

impl SupervisorRpc {
    pub const fn new(manager: Manager) -> Self {
        Self { manager }
    }
}

type AttachStream = Pin<Box<dyn Stream<Item = Result<AttachResponse, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl SupervisorControlService for SupervisorRpc {
    type AttachStream = AttachStream;

    async fn attach(
        &self,
        request: Request<Streaming<AttachRequest>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        let caller = request
            .extensions()
            .get::<CallerIdentity>()
            .cloned()
            .ok_or_else(|| Status::unauthenticated("named-pipe caller identity is missing"))?;
        let mut inbound = request.into_inner();
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("supervisor stream is empty"))?;
        let hello = match first.message {
            Some(attach_request::Message::Hello(hello)) => hello,
            _ => {
                return Err(Status::invalid_argument(
                    "first supervisor message must be Hello",
                ));
            }
        };
        uuid::Uuid::parse_str(&hello.execution_id)
            .map_err(|_| Status::invalid_argument("execution_id must be a UUID"))?;
        let mut attachment = self
            .manager
            .attach_supervisor(
                &hello.execution_id,
                &hello.workload_id,
                &hello.manager_session_id,
                &hello.execution_config_hash,
                caller.process_id,
            )
            .map_err(map_error)?;
        let (sender, receiver) = mpsc::channel(16);
        sender
            .send(Ok(AttachResponse {
                welcome: Some(Welcome {
                    execution_configuration: Some(attachment.configuration),
                    committed_sequence: attachment.committed_sequence,
                }),
                command: None,
                acknowledgement: None,
            }))
            .await
            .map_err(|_| Status::cancelled("supervisor disconnected"))?;
        let manager = self.manager.clone();
        let execution_id = hello.execution_id;
        let attachment_id = attachment.attachment_id.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    command = attachment.commands.recv() => {
                        let Ok(command) = command else {
                            break;
                        };
                        if sender.send(Ok(AttachResponse {
                            welcome: None,
                            command: Some(SupervisorCommand {
                                kind: command.kind,
                                payload: command.payload,
                            }),
                            acknowledgement: None,
                        })).await.is_err() {
                            break;
                        }
                    }
                    message = inbound.message() => {
                        let Ok(Some(message)) = message else {
                            break;
                        };
                        let Some(attach_request::Message::Observation(observation)) = message.message else {
                            continue;
                        };
                        if !matches!(
                            observation.kind.as_str(),
                            "attempt-exited"
                                | "attempt-started"
                                | "launch-failed"
                                | "supervisor-lost"
                                | "restart-backoff"
                                | "completed"
                                | "failed"
                                | "stopped"
                                | "cancelled"
                                | "outcome-unknown"
                                | "policy-applied"
                        ) {
                            let _ = sender.send(Err(Status::invalid_argument(
                                "unknown supervisor observation kind",
                            ))).await;
                            break;
                        }
                        let sequence = observation.sequence;
                        if let Err(error) = manager.record_supervisor_observation(&execution_id, SupervisorObservation {
                            sequence,
                            kind: observation.kind,
                            exit_code: observation.exit_code,
                            detail: observation.detail,
                            attempt: observation.attempt,
                            workload_process_id: observation.workload_process_id,
                        }) {
                            let _ = sender.send(Err(map_error(error))).await;
                            break;
                        }
                        if sender.send(Ok(AttachResponse {
                            welcome: None,
                            command: None,
                            acknowledgement: Some(Acknowledgement {
                                committed_sequence: sequence,
                            }),
                        })).await.is_err() {
                            break;
                        }
                    }
                }
            }
            manager.supervisor_disconnected(&execution_id, &attachment_id);
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

fn map_error(error: ManagerError) -> Status {
    match error {
        ManagerError::NotFound(_) => Status::not_found(error.to_string()),
        ManagerError::DefinitionMissing(_)
        | ManagerError::WrongKind { .. }
        | ManagerError::KindChangeActive(_)
        | ManagerError::SupervisorIdentityMismatch
        | ManagerError::SessionEnding => Status::failed_precondition(error.to_string()),
        ManagerError::AlreadyActive(_) => Status::already_exists(error.to_string()),
        ManagerError::Config(_)
        | ManagerError::Storage(_)
        | ManagerError::Encode(_)
        | ManagerError::Decode(_)
        | ManagerError::Io { .. }
        | ManagerError::InvalidSystemTime => Status::internal(error.to_string()),
    }
}
