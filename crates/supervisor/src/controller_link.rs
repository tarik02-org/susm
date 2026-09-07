use std::{io, time::Duration};

use prost::Message;
use susm_protocol::{
    MAX_MESSAGE_SIZE,
    pipe::{connect, current_user_sid, supervisor_pipe_name},
    runtime::RuntimeObservation,
    supervisor::{
        AttachRequest, ExecutionConfiguration, Hello, Observation, PolicyUpdate, attach_request,
        supervisor_control_service_client::SupervisorControlServiceClient,
    },
};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;

use crate::{journal::RuntimeJournal, runner::SupervisorIdentity};

pub async fn run(
    configuration: ExecutionConfiguration,
    identity: SupervisorIdentity,
    journal: RuntimeJournal,
    stop: watch::Sender<bool>,
    policy: watch::Sender<Option<PolicyUpdate>>,
    mut live_observations: mpsc::UnboundedReceiver<RuntimeObservation>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sid = current_user_sid()?;
    loop {
        let channel = match connect(supervisor_pipe_name(
            &sid,
            &configuration.manager_session_id,
        ))
        .await
        {
            Ok(channel) => channel,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        let mut client = SupervisorControlServiceClient::new(channel)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE);
        let (requests, receiver) = tokio::sync::mpsc::channel(32);
        let recovery = journal.recovery();
        requests
            .send(AttachRequest {
                message: Some(attach_request::Message::Hello(Hello {
                    manager_session_id: configuration.manager_session_id.clone(),
                    workload_id: configuration.workload_id.clone(),
                    execution_id: configuration.execution_id.clone(),
                    supervisor_id: identity.id.clone(),
                    incarnation: identity.incarnation,
                    execution_config_hash: configuration.execution_config_hash.clone(),
                    last_sequence: recovery.last_sequence,
                })),
            })
            .await
            .map_err(|_| io::Error::other("supervisor request stream closed"))?;
        let mut responses = match client.attach(ReceiverStream::new(receiver)).await {
            Ok(response) => response.into_inner(),
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        let Some(welcome) = responses
            .message()
            .await?
            .and_then(|response| response.welcome)
        else {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        };
        let committed = welcome.committed_sequence;
        if committed > recovery.committed_sequence {
            journal.acknowledge(committed)?;
        }
        let mut sent_sequence = committed;
        let mut terminal_sequence = None;
        for observation in journal.observations_after(committed) {
            send_observation(&requests, &observation).await?;
            sent_sequence = observation.sequence;
            if observation.terminal {
                terminal_sequence = Some(observation.sequence);
            }
        }

        let mut observations_open = true;
        loop {
            tokio::select! {
                response = responses.message() => {
                    let Ok(Some(response)) = response else {
                        break;
                    };
                    if let Some(command) = response.command {
                        match command.kind.as_str() {
                            "stop" | "session-ending" => {
                                stop.send_replace(true);
                            }
                            "update-policy" => {
                                if let Ok(update) = PolicyUpdate::decode(command.payload.as_slice()) {
                                    policy.send_replace(Some(update));
                                }
                            }
                            _ => {}
                        }
                    }
                    if let Some(acknowledgement) = response.acknowledgement {
                        journal.acknowledge(acknowledgement.committed_sequence)?;
                        if terminal_sequence.is_some_and(|terminal| {
                            acknowledgement.committed_sequence >= terminal
                        }) {
                            return Ok(());
                        }
                    }
                }
                observation = live_observations.recv(), if observations_open => {
                    let Some(observation) = observation else {
                        observations_open = false;
                        if terminal_sequence.is_none() {
                            terminal_sequence = journal
                                .observations_after(0)
                                .into_iter()
                                .rev()
                                .find(|observation| observation.terminal)
                                .map(|observation| observation.sequence);
                        }
                        continue;
                    };
                    if observation.sequence <= sent_sequence {
                        continue;
                    }
                    send_observation(&requests, &observation).await?;
                    sent_sequence = observation.sequence;
                    if observation.terminal {
                        terminal_sequence = Some(observation.sequence);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn send_observation(
    requests: &mpsc::Sender<AttachRequest>,
    observation: &RuntimeObservation,
) -> Result<(), io::Error> {
    requests
        .send(AttachRequest {
            message: Some(attach_request::Message::Observation(Observation {
                sequence: observation.sequence,
                kind: observation.kind.clone(),
                exit_code: observation.exit_code,
                detail: observation.detail.clone(),
                attempt: observation.attempt,
                workload_process_id: observation.workload_process_id,
            })),
        })
        .await
        .map_err(|_| io::Error::other("supervisor request stream closed"))
}
