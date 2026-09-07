use std::collections::BTreeMap;
use std::time::Duration;

use sha2::{Digest, Sha256};
use susm_domain::ids::{ConfigGeneration, ProcessDefinitionHash, WorkloadId};
use susm_domain::restart::{
    BackoffPolicy, ExecutionRestartPolicy, JobRestartPolicy, ServiceRestartPolicy,
    ServiceRetryLimit,
};

use crate::model::{
    LoggingPolicy, ProcessDefinition, RetentionDuration, RetentionLimit, StopDefinition,
    WorkloadDefinition, WorkloadKind,
};

pub(crate) fn process_hash(process: &ProcessDefinition) -> ProcessDefinitionHash {
    let mut writer = HashWriter::new(b"susm-process-definition-v1");
    write_process(&mut writer, process);
    ProcessDefinitionHash::from_sha256(writer.finish())
}

pub(crate) fn config_generation(
    definitions: &BTreeMap<WorkloadId, WorkloadDefinition>,
) -> ConfigGeneration {
    let mut writer = HashWriter::new(b"susm-config-generation-v1");
    writer.usize(definitions.len());
    for (id, definition) in definitions {
        writer.string(id.as_str());
        write_definition(&mut writer, definition);
    }
    ConfigGeneration::from_sha256(writer.finish())
}

fn write_definition(writer: &mut HashWriter, definition: &WorkloadDefinition) {
    writer.tag(match definition.kind() {
        WorkloadKind::Service => 1,
        WorkloadKind::Job => 2,
    });
    write_process(writer, definition.process());

    writer.usize(definition.success_exit_codes().len());
    for exit_code in definition.success_exit_codes() {
        writer.u32(*exit_code);
    }

    write_restart(writer, definition.restart());
    write_stop(writer, definition.stop());
    write_logging(writer, definition.logging());
}

fn write_process(writer: &mut HashWriter, process: &ProcessDefinition) {
    writer.string(process.executable());
    writer.usize(process.arguments().len());
    for argument in process.arguments() {
        writer.string(argument);
    }
    writer.string(process.working_directory());

    writer.usize(process.environment().set().len());
    for (name, value) in process.environment().set() {
        writer.string(name.as_str());
        writer.string(value);
    }
    writer.usize(process.environment().unset().len());
    for name in process.environment().unset() {
        writer.string(name.as_str());
    }
}

fn write_restart(writer: &mut HashWriter, restart: ExecutionRestartPolicy) {
    match restart {
        ExecutionRestartPolicy::Service(ServiceRestartPolicy::Never) => writer.tag(1),
        ExecutionRestartPolicy::Service(ServiceRestartPolicy::OnFailure(policy)) => {
            writer.tag(2);
            write_service_retry(
                writer,
                policy.limit(),
                policy.reset_after().get(),
                policy.backoff(),
            );
        }
        ExecutionRestartPolicy::Service(ServiceRestartPolicy::Always(policy)) => {
            writer.tag(3);
            write_service_retry(
                writer,
                policy.limit(),
                policy.reset_after().get(),
                policy.backoff(),
            );
        }
        ExecutionRestartPolicy::Job(JobRestartPolicy::Never) => writer.tag(4),
        ExecutionRestartPolicy::Job(JobRestartPolicy::OnFailure(policy)) => {
            writer.tag(5);
            writer.u32(policy.max_restarts().get());
            write_backoff(writer, policy.backoff());
        }
    }
}

fn write_service_retry(
    writer: &mut HashWriter,
    limit: ServiceRetryLimit,
    reset_after: Duration,
    backoff: BackoffPolicy,
) {
    match limit {
        ServiceRetryLimit::Finite(limit) => {
            writer.tag(1);
            writer.u32(limit.get());
        }
        ServiceRetryLimit::Unlimited => writer.tag(2),
    }
    writer.duration(reset_after);
    write_backoff(writer, backoff);
}

fn write_backoff(writer: &mut HashWriter, backoff: BackoffPolicy) {
    writer.duration(backoff.initial().get());
    writer.u32(backoff.multiplier().get());
    writer.duration(backoff.maximum().get());
}

fn write_stop(writer: &mut HashWriter, stop: &StopDefinition) {
    match stop {
        StopDefinition::CtrlBreak { timeout } => {
            writer.tag(1);
            writer.duration(*timeout);
        }
        StopDefinition::Command { timeout, command } => {
            writer.tag(2);
            writer.duration(*timeout);
            writer.string(command.executable());
            writer.usize(command.arguments().len());
            for argument in command.arguments() {
                writer.string(argument);
            }
        }
        StopDefinition::Kill => writer.tag(3),
    }
}

fn write_logging(writer: &mut HashWriter, logging: LoggingPolicy) {
    match logging {
        LoggingPolicy::Disabled => writer.tag(1),
        LoggingPolicy::Capture {
            segment_size,
            segment_age,
            retention_size,
            retention_age,
        } => {
            writer.tag(2);
            writer.u64(segment_size);
            writer.duration(segment_age);
            match retention_size {
                RetentionLimit::Limited(size) => {
                    writer.tag(1);
                    writer.u64(size);
                }
                RetentionLimit::Unlimited => writer.tag(2),
            }
            match retention_age {
                RetentionDuration::Limited(age) => {
                    writer.tag(1);
                    writer.duration(age);
                }
                RetentionDuration::Unlimited => writer.tag(2),
            }
        }
    }
}

struct HashWriter(Sha256);

impl HashWriter {
    fn new(format: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(format.len().to_le_bytes());
        digest.update(format);
        Self(digest)
    }

    fn tag(&mut self, value: u8) {
        self.0.update([value]);
    }

    fn u32(&mut self, value: u32) {
        self.0.update(value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.update(value.to_le_bytes());
    }

    fn usize(&mut self, value: usize) {
        self.u64(u64::try_from(value).expect("in-memory collection length fits u64"));
    }

    fn duration(&mut self, value: Duration) {
        self.0.update(value.as_nanos().to_le_bytes());
    }

    fn string(&mut self, value: &str) {
        self.usize(value.len());
        self.0.update(value.as_bytes());
    }

    fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}
