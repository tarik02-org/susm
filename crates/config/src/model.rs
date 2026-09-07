use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};
use std::time::Duration;

use susm_domain::ids::{ConfigGeneration, ProcessDefinitionHash, WorkloadId};
use susm_domain::restart::ExecutionRestartPolicy;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WorkloadKind {
    Service,
    Job,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateConfig {
    generation: ConfigGeneration,
    definitions: BTreeMap<WorkloadId, WorkloadDefinition>,
}

impl CandidateConfig {
    pub(crate) const fn new(
        generation: ConfigGeneration,
        definitions: BTreeMap<WorkloadId, WorkloadDefinition>,
    ) -> Self {
        Self {
            generation,
            definitions,
        }
    }

    pub const fn generation(&self) -> ConfigGeneration {
        self.generation
    }

    pub const fn definitions(&self) -> &BTreeMap<WorkloadId, WorkloadDefinition> {
        &self.definitions
    }

    pub fn into_definitions(self) -> BTreeMap<WorkloadId, WorkloadDefinition> {
        self.definitions
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkloadDefinition {
    pub(crate) id: WorkloadId,
    pub(crate) kind: WorkloadKind,
    pub(crate) process: ProcessDefinition,
    pub(crate) process_hash: ProcessDefinitionHash,
    pub(crate) success_exit_codes: BTreeSet<u32>,
    pub(crate) restart: ExecutionRestartPolicy,
    pub(crate) stop: StopDefinition,
    pub(crate) logging: LoggingPolicy,
}

impl WorkloadDefinition {
    pub const fn id(&self) -> &WorkloadId {
        &self.id
    }

    pub const fn kind(&self) -> WorkloadKind {
        self.kind
    }

    pub const fn process(&self) -> &ProcessDefinition {
        &self.process
    }

    pub const fn process_hash(&self) -> ProcessDefinitionHash {
        self.process_hash
    }

    pub const fn success_exit_codes(&self) -> &BTreeSet<u32> {
        &self.success_exit_codes
    }

    pub const fn restart(&self) -> ExecutionRestartPolicy {
        self.restart
    }

    pub const fn stop(&self) -> &StopDefinition {
        &self.stop
    }

    pub const fn logging(&self) -> LoggingPolicy {
        self.logging
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessDefinition {
    executable: Box<str>,
    arguments: Vec<Box<str>>,
    working_directory: Box<str>,
    environment: EnvironmentOverlay,
}

impl ProcessDefinition {
    pub(crate) const fn new(
        executable: Box<str>,
        arguments: Vec<Box<str>>,
        working_directory: Box<str>,
        environment: EnvironmentOverlay,
    ) -> Self {
        Self {
            executable,
            arguments,
            working_directory,
            environment,
        }
    }

    pub const fn executable(&self) -> &str {
        &self.executable
    }

    pub fn arguments(&self) -> impl ExactSizeIterator<Item = &str> {
        self.arguments.iter().map(AsRef::as_ref)
    }

    pub const fn working_directory(&self) -> &str {
        &self.working_directory
    }

    pub const fn environment(&self) -> &EnvironmentOverlay {
        &self.environment
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EnvironmentName(Box<str>);

impl EnvironmentName {
    pub(crate) fn parse(value: &str) -> Result<Self, InvalidEnvironmentName> {
        if value.is_empty() {
            return Err(InvalidEnvironmentName::Empty);
        }
        if value.contains('=') {
            return Err(InvalidEnvironmentName::ContainsEquals);
        }
        if value.contains('\0') {
            return Err(InvalidEnvironmentName::ContainsNull);
        }

        Ok(Self(value.to_uppercase().into_boxed_str()))
    }

    pub const fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for EnvironmentName {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidEnvironmentName {
    Empty,
    ContainsEquals,
    ContainsNull,
}

impl Display for InvalidEnvironmentName {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("environment variable name is empty"),
            Self::ContainsEquals => formatter.write_str("environment variable name contains '='"),
            Self::ContainsNull => {
                formatter.write_str("environment variable name contains a null character")
            }
        }
    }
}

impl std::error::Error for InvalidEnvironmentName {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnvironmentOverlay {
    set: BTreeMap<EnvironmentName, Box<str>>,
    unset: BTreeSet<EnvironmentName>,
}

impl EnvironmentOverlay {
    pub(crate) const fn new(
        set: BTreeMap<EnvironmentName, Box<str>>,
        unset: BTreeSet<EnvironmentName>,
    ) -> Self {
        Self { set, unset }
    }

    pub const fn set(&self) -> &BTreeMap<EnvironmentName, Box<str>> {
        &self.set
    }

    pub const fn unset(&self) -> &BTreeSet<EnvironmentName> {
        &self.unset
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StopDefinition {
    CtrlBreak {
        timeout: Duration,
    },
    Command {
        timeout: Duration,
        command: StopCommand,
    },
    Kill,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopCommand {
    executable: Box<str>,
    arguments: Vec<Box<str>>,
}

impl StopCommand {
    pub(crate) const fn new(executable: Box<str>, arguments: Vec<Box<str>>) -> Self {
        Self {
            executable,
            arguments,
        }
    }

    pub const fn executable(&self) -> &str {
        &self.executable
    }

    pub fn arguments(&self) -> impl ExactSizeIterator<Item = &str> {
        self.arguments.iter().map(AsRef::as_ref)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoggingPolicy {
    Disabled,
    Capture {
        segment_size: u64,
        segment_age: Duration,
        retention_size: RetentionLimit,
        retention_age: RetentionDuration,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionLimit {
    Limited(u64),
    Unlimited,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionDuration {
    Limited(Duration),
    Unlimited,
}
