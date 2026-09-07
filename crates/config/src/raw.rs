use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::path::Path;
use std::time::Duration;

use parse_size::{ByteSuffix, Config as SizeParser};
use serde::Deserialize;
use susm_domain::ids::WorkloadId;
use susm_domain::restart::{
    BackoffPolicy, DEFAULT_BACKOFF_INITIAL, DEFAULT_BACKOFF_MAXIMUM, DEFAULT_BACKOFF_MULTIPLIER,
    DEFAULT_SERVICE_MAX_RESTARTS, DEFAULT_SERVICE_RESET_AFTER, ExecutionRestartPolicy,
    JobRestartPolicy, JobRetryPolicy, ServiceRestartPolicy, ServiceRetryLimit, ServiceRetryPolicy,
};
use susm_domain::time::NonZeroDuration;
use thiserror::Error;

use crate::canonical::process_hash;
use crate::model::{
    EnvironmentName, EnvironmentOverlay, LoggingPolicy, ProcessDefinition, RetentionDuration,
    RetentionLimit, StopCommand, StopDefinition, WorkloadDefinition, WorkloadKind,
};

const DEFAULT_WORKING_DIRECTORY: &str = "${USERPROFILE}";
const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SEGMENT_SIZE: &str = "16MiB";
const DEFAULT_SEGMENT_AGE: Duration = Duration::from_secs(60 * 60);
const DEFAULT_RETENTION_SIZE: &str = "1GiB";
const DEFAULT_RETENTION_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Error)]
pub enum DefinitionError {
    #[error("invalid `{field}`: {reason}")]
    InvalidField { field: &'static str, reason: String },
    #[error("`{field}` is not allowed when {context}")]
    FieldNotAllowed {
        field: &'static str,
        context: &'static str,
    },
    #[error("`{field}` is required when {context}")]
    FieldRequired {
        field: &'static str,
        context: &'static str,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDefinition {
    kind: RawKind,
    executable: String,
    #[serde(default)]
    arguments: Vec<String>,
    working_directory: Option<String>,
    success_exit_codes: Option<Vec<u32>>,
    #[serde(default)]
    environment: RawEnvironment,
    restart: Option<RawRestart>,
    stop: Option<RawStop>,
    logging: Option<RawLogging>,
}

impl RawDefinition {
    pub(crate) fn parse(source: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(source)
    }

    pub(crate) fn into_definition(
        self,
        id: WorkloadId,
    ) -> Result<WorkloadDefinition, DefinitionError> {
        validate_executable("executable", &self.executable)?;
        validate_arguments("arguments", &self.arguments)?;
        let working_directory = self
            .working_directory
            .unwrap_or_else(|| DEFAULT_WORKING_DIRECTORY.to_owned());
        validate_text("working_directory", &working_directory, false)?;

        let kind = match self.kind {
            RawKind::Service => WorkloadKind::Service,
            RawKind::Job => WorkloadKind::Job,
        };
        let process = ProcessDefinition::new(
            self.executable.into_boxed_str(),
            self.arguments
                .into_iter()
                .map(String::into_boxed_str)
                .collect(),
            working_directory.into_boxed_str(),
            self.environment.into_overlay()?,
        );
        let process_hash = process_hash(&process);
        let success_exit_codes = self
            .success_exit_codes
            .unwrap_or_else(|| vec![0])
            .into_iter()
            .collect();
        let restart = restart_policy(kind, self.restart)?;
        let stop = stop_definition(self.stop)?;
        let logging = logging_policy(self.logging)?;

        Ok(WorkloadDefinition {
            id,
            kind,
            process,
            process_hash,
            success_exit_codes,
            restart,
            stop,
            logging,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawKind {
    Service,
    Job,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnvironment {
    #[serde(default)]
    set: BTreeMap<String, String>,
    #[serde(default)]
    unset: Vec<String>,
}

impl RawEnvironment {
    fn into_overlay(self) -> Result<EnvironmentOverlay, DefinitionError> {
        let mut set = BTreeMap::new();
        for (raw_name, value) in self.set {
            validate_text("environment.set", &value, true)?;
            let name = EnvironmentName::parse(&raw_name)
                .map_err(|error| invalid("environment.set", format!("{raw_name:?}: {error}")))?;
            if set.insert(name, value.into_boxed_str()).is_some() {
                return Err(invalid(
                    "environment.set",
                    format!("duplicate Windows environment name {raw_name:?}"),
                ));
            }
        }

        let mut unset = BTreeSet::new();
        for raw_name in self.unset {
            let name = EnvironmentName::parse(&raw_name)
                .map_err(|error| invalid("environment.unset", format!("{raw_name:?}: {error}")))?;
            if !unset.insert(name.clone()) {
                return Err(invalid(
                    "environment.unset",
                    format!("duplicate Windows environment name {raw_name:?}"),
                ));
            }
            if set.contains_key(&name) {
                return Err(invalid(
                    "environment",
                    format!("{} appears in both set and unset", name.as_str()),
                ));
            }
        }

        Ok(EnvironmentOverlay::new(set, unset))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRestart {
    policy: Option<RawRestartPolicy>,
    max_restarts: Option<RawRestartLimit>,
    #[serde(default, with = "humantime_serde::option")]
    reset_after: Option<Duration>,
    backoff: Option<RawBackoff>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawRestartPolicy {
    Never,
    OnFailure,
    Always,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawRestartLimit {
    Finite(u32),
    Keyword(String),
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackoff {
    #[serde(default, with = "humantime_serde::option")]
    initial: Option<Duration>,
    multiplier: Option<u32>,
    #[serde(default, with = "humantime_serde::option")]
    maximum: Option<Duration>,
}

impl RawBackoff {
    fn into_policy(self) -> Result<BackoffPolicy, DefinitionError> {
        let initial = non_zero_duration(
            "restart.backoff.initial",
            self.initial.unwrap_or(DEFAULT_BACKOFF_INITIAL),
        )?;
        let maximum = non_zero_duration(
            "restart.backoff.maximum",
            self.maximum.unwrap_or(DEFAULT_BACKOFF_MAXIMUM),
        )?;
        let multiplier = NonZeroU32::new(self.multiplier.unwrap_or(DEFAULT_BACKOFF_MULTIPLIER))
            .ok_or_else(|| invalid("restart.backoff.multiplier", "must be positive"))?;
        BackoffPolicy::new(initial, multiplier, maximum)
            .map_err(|error| invalid("restart.backoff", error.to_string()))
    }
}

fn restart_policy(
    kind: WorkloadKind,
    raw: Option<RawRestart>,
) -> Result<ExecutionRestartPolicy, DefinitionError> {
    let raw = raw.unwrap_or_default();
    match kind {
        WorkloadKind::Service => service_restart_policy(raw),
        WorkloadKind::Job => job_restart_policy(raw),
    }
}

fn service_restart_policy(raw: RawRestart) -> Result<ExecutionRestartPolicy, DefinitionError> {
    let policy = raw.policy.unwrap_or(RawRestartPolicy::OnFailure);
    if matches!(policy, RawRestartPolicy::Never) {
        reject_retry_fields(&raw, "restart.policy is never")?;
        return Ok(ExecutionRestartPolicy::Service(ServiceRestartPolicy::Never));
    }

    let limit = match raw.max_restarts {
        None => ServiceRetryLimit::Finite(
            NonZeroU32::new(DEFAULT_SERVICE_MAX_RESTARTS)
                .expect("default service restart limit is positive"),
        ),
        Some(RawRestartLimit::Finite(0)) => {
            return Ok(ExecutionRestartPolicy::Service(ServiceRestartPolicy::Never));
        }
        Some(RawRestartLimit::Finite(value)) => ServiceRetryLimit::Finite(
            NonZeroU32::new(value).expect("non-zero match arm contains a positive value"),
        ),
        Some(RawRestartLimit::Keyword(keyword)) if keyword == "unlimited" => {
            ServiceRetryLimit::Unlimited
        }
        Some(RawRestartLimit::Keyword(keyword)) => {
            return Err(invalid(
                "restart.max_restarts",
                format!("unknown value {keyword:?}"),
            ));
        }
    };
    let reset_after = non_zero_duration(
        "restart.reset_after",
        raw.reset_after.unwrap_or(DEFAULT_SERVICE_RESET_AFTER),
    )?;
    let backoff = raw.backoff.unwrap_or_default().into_policy()?;
    let retry = ServiceRetryPolicy::new(limit, reset_after, backoff);
    Ok(ExecutionRestartPolicy::Service(match policy {
        RawRestartPolicy::OnFailure => ServiceRestartPolicy::OnFailure(retry),
        RawRestartPolicy::Always => ServiceRestartPolicy::Always(retry),
        RawRestartPolicy::Never => unreachable!("never returned before retry policy creation"),
    }))
}

fn job_restart_policy(raw: RawRestart) -> Result<ExecutionRestartPolicy, DefinitionError> {
    match raw.policy.unwrap_or(RawRestartPolicy::Never) {
        RawRestartPolicy::Never => {
            reject_retry_fields(&raw, "restart.policy is never")?;
            Ok(ExecutionRestartPolicy::Job(JobRestartPolicy::Never))
        }
        RawRestartPolicy::Always => Err(invalid("restart.policy", "jobs do not support always")),
        RawRestartPolicy::OnFailure => {
            if raw.reset_after.is_some() {
                return Err(DefinitionError::FieldNotAllowed {
                    field: "restart.reset_after",
                    context: "the workload is a job",
                });
            }
            let max_restarts = match raw.max_restarts {
                Some(RawRestartLimit::Finite(0)) => {
                    return Ok(ExecutionRestartPolicy::Job(JobRestartPolicy::Never));
                }
                Some(RawRestartLimit::Finite(value)) => {
                    NonZeroU32::new(value).expect("non-zero match arm contains a positive value")
                }
                Some(RawRestartLimit::Keyword(keyword)) => {
                    return Err(invalid(
                        "restart.max_restarts",
                        format!("jobs require a finite integer, got {keyword:?}"),
                    ));
                }
                None => {
                    return Err(DefinitionError::FieldRequired {
                        field: "restart.max_restarts",
                        context: "a job uses on-failure restart",
                    });
                }
            };
            let backoff = raw.backoff.unwrap_or_default().into_policy()?;
            Ok(ExecutionRestartPolicy::Job(JobRestartPolicy::OnFailure(
                JobRetryPolicy::new(max_restarts, backoff),
            )))
        }
    }
}

fn reject_retry_fields(raw: &RawRestart, context: &'static str) -> Result<(), DefinitionError> {
    for (field, present) in [
        ("restart.max_restarts", raw.max_restarts.is_some()),
        ("restart.reset_after", raw.reset_after.is_some()),
        ("restart.backoff", raw.backoff.is_some()),
    ] {
        if present {
            return Err(DefinitionError::FieldNotAllowed { field, context });
        }
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStop {
    method: Option<RawStopMethod>,
    #[serde(default, with = "humantime_serde::option")]
    timeout: Option<Duration>,
    command: Option<RawStopCommand>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawStopMethod {
    CtrlBreak,
    Command,
    Kill,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStopCommand {
    executable: String,
    #[serde(default)]
    arguments: Vec<String>,
}

fn stop_definition(raw: Option<RawStop>) -> Result<StopDefinition, DefinitionError> {
    let raw = raw.unwrap_or_default();
    match raw.method.unwrap_or(RawStopMethod::CtrlBreak) {
        RawStopMethod::CtrlBreak => {
            if raw.command.is_some() {
                return Err(DefinitionError::FieldNotAllowed {
                    field: "stop.command",
                    context: "stop.method is ctrl-break",
                });
            }
            Ok(StopDefinition::CtrlBreak {
                timeout: non_zero_duration(
                    "stop.timeout",
                    raw.timeout.unwrap_or(DEFAULT_STOP_TIMEOUT),
                )?
                .get(),
            })
        }
        RawStopMethod::Command => {
            let timeout = raw.timeout.ok_or(DefinitionError::FieldRequired {
                field: "stop.timeout",
                context: "stop.method is command",
            })?;
            let command = raw.command.ok_or(DefinitionError::FieldRequired {
                field: "stop.command",
                context: "stop.method is command",
            })?;
            validate_executable("stop.command.executable", &command.executable)?;
            validate_arguments("stop.command.arguments", &command.arguments)?;
            Ok(StopDefinition::Command {
                timeout: non_zero_duration("stop.timeout", timeout)?.get(),
                command: StopCommand::new(
                    command.executable.into_boxed_str(),
                    command
                        .arguments
                        .into_iter()
                        .map(String::into_boxed_str)
                        .collect(),
                ),
            })
        }
        RawStopMethod::Kill => {
            if raw.timeout.is_some() {
                return Err(DefinitionError::FieldNotAllowed {
                    field: "stop.timeout",
                    context: "stop.method is kill",
                });
            }
            if raw.command.is_some() {
                return Err(DefinitionError::FieldNotAllowed {
                    field: "stop.command",
                    context: "stop.method is kill",
                });
            }
            Ok(StopDefinition::Kill)
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLogging {
    capture: Option<bool>,
    segment_size: Option<String>,
    #[serde(default, with = "humantime_serde::option")]
    segment_age: Option<Duration>,
    retention_size: Option<String>,
    retention_age: Option<RawRetentionAge>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawRetentionAge {
    Value(#[serde(with = "humantime_serde")] Duration),
    Keyword(String),
}

fn logging_policy(raw: Option<RawLogging>) -> Result<LoggingPolicy, DefinitionError> {
    let raw = raw.unwrap_or_default();
    if !raw.capture.unwrap_or(true) {
        for (field, present) in [
            ("logging.segment_size", raw.segment_size.is_some()),
            ("logging.segment_age", raw.segment_age.is_some()),
            ("logging.retention_size", raw.retention_size.is_some()),
            ("logging.retention_age", raw.retention_age.is_some()),
        ] {
            if present {
                return Err(DefinitionError::FieldNotAllowed {
                    field,
                    context: "logging.capture is false",
                });
            }
        }
        return Ok(LoggingPolicy::Disabled);
    }

    let segment_size = parse_positive_size(
        "logging.segment_size",
        raw.segment_size.as_deref().unwrap_or(DEFAULT_SEGMENT_SIZE),
    )?;
    let segment_age = non_zero_duration(
        "logging.segment_age",
        raw.segment_age.unwrap_or(DEFAULT_SEGMENT_AGE),
    )?
    .get();
    let retention_size = match raw.retention_size.as_deref() {
        Some("unlimited") => RetentionLimit::Unlimited,
        Some(value) => {
            RetentionLimit::Limited(parse_positive_size("logging.retention_size", value)?)
        }
        None => RetentionLimit::Limited(parse_positive_size(
            "logging.retention_size",
            DEFAULT_RETENTION_SIZE,
        )?),
    };
    let retention_age = match raw.retention_age {
        Some(RawRetentionAge::Keyword(keyword)) if keyword == "unlimited" => {
            RetentionDuration::Unlimited
        }
        Some(RawRetentionAge::Keyword(keyword)) => {
            return Err(invalid(
                "logging.retention_age",
                format!("unknown value {keyword:?}"),
            ));
        }
        Some(RawRetentionAge::Value(value)) => {
            RetentionDuration::Limited(non_zero_duration("logging.retention_age", value)?.get())
        }
        None => RetentionDuration::Limited(DEFAULT_RETENTION_AGE),
    };

    Ok(LoggingPolicy::Capture {
        segment_size,
        segment_age,
        retention_size,
        retention_age,
    })
}

fn parse_positive_size(field: &'static str, value: &str) -> Result<u64, DefinitionError> {
    let parser = SizeParser::new()
        .with_binary()
        .with_byte_suffix(ByteSuffix::Require);
    let bytes = parser
        .parse_size(value)
        .map_err(|error| invalid(field, error.to_string()))?;
    if bytes == 0 {
        return Err(invalid(field, "must be positive"));
    }
    Ok(bytes)
}

fn validate_executable(field: &'static str, value: &str) -> Result<(), DefinitionError> {
    validate_text(field, value, false)?;
    if let Some(extension) = Path::new(value)
        .extension()
        .and_then(|value| value.to_str())
        && ["cmd", "bat", "ps1"]
            .iter()
            .any(|blocked| extension.eq_ignore_ascii_case(blocked))
    {
        return Err(invalid(
            field,
            format!(".{extension} requires an explicit shell executable"),
        ));
    }
    Ok(())
}

fn validate_arguments(field: &'static str, values: &[String]) -> Result<(), DefinitionError> {
    for value in values {
        validate_text(field, value, true)?;
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    allow_empty: bool,
) -> Result<(), DefinitionError> {
    if !allow_empty && value.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if value.contains('\0') {
        return Err(invalid(field, "must not contain a null character"));
    }
    Ok(())
}

fn non_zero_duration(
    field: &'static str,
    value: Duration,
) -> Result<NonZeroDuration, DefinitionError> {
    NonZeroDuration::new(value).ok_or_else(|| invalid(field, "must be positive"))
}

fn invalid(field: &'static str, reason: impl Into<String>) -> DefinitionError {
    DefinitionError::InvalidField {
        field,
        reason: reason.into(),
    }
}
