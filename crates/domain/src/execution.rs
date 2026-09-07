use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};
use std::time::Duration;

use crate::ids::{
    AttemptNumber, ExecutionConfigHash, ExecutionId, IntentRevision, ManagerSessionId,
    ProcessDefinitionHash, WorkloadId,
};
use crate::restart::{ExecutionRestartPolicy, RetryOrdinal};
use crate::time::{BootInstant, RetryDeadline, StopDeadline};
use crate::transition::Transition;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionState {
    pub id: ExecutionId,
    pub workload_id: WorkloadId,
    pub origin: ExecutionOrigin,
    pub source_definition: ProcessDefinitionHash,
    pub execution_config: ExecutionConfigHash,
    pub success_exit_codes: BTreeSet<ExitCode>,
    pub restart_policy: ExecutionRestartPolicy,
    pub last_retry: Option<RetryOrdinal>,
    pub phase: ExecutionPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionOrigin {
    ServiceIntent { revision: IntentRevision },
    Job(JobExecutionOrigin),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobExecutionOrigin {
    Manual,
    Enabled {
        manager_session_id: ManagerSessionId,
    },
    Rerun {
        previous_execution_id: ExecutionId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionPhase {
    Launching {
        attempt: AttemptNumber,
    },
    Running {
        attempt: AttemptNumber,
    },
    RestartBackoff {
        previous_attempt: AttemptNumber,
        previous_end: AttemptEnd,
        next_attempt: AttemptNumber,
        retry: RetryOrdinal,
        retry_at: RetryDeadline,
    },
    Stopping {
        attempt: AttemptNumber,
        phase: StopPhase,
        cause: StopCause,
        plan: StopPlan,
    },
    Terminal {
        outcome: ExecutionOutcome,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopPhase {
    AwaitingLaunch { deadline: StopDeadline },
    Graceful { deadline: StopDeadline },
    Killing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopPlan {
    CtrlBreak { timeout: Duration },
    Command { timeout: Duration },
    Kill,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceStopCause {
    Stop,
    Restart,
    ManagerSessionEnded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobCancelCause {
    Cancel,
    Rerun,
    ManagerSessionEnded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopCause {
    Service(ServiceStopCause),
    Job(JobCancelCause),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptEnd {
    LaunchFailed {
        failure: LaunchFailure,
    },
    Exited {
        exit_code: ExitCode,
        ran_for: Duration,
    },
    Killed,
    SupervisorLost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchFailure {
    Preflight(PreflightFailure),
    Win32(Win32ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreflightFailure {
    EnvironmentVariableMissing,
    ExecutableNotFound,
    WorkingDirectoryUnavailable,
    CommandLineTooLong,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionOutcome {
    Completed {
        exit_code: ExitCode,
    },
    Failed {
        failure: ExecutionFailure,
    },
    OutcomeUnknown {
        failure: InfrastructureFailure,
    },
    Stopped {
        cause: ServiceStopCause,
        forced: bool,
    },
    Cancelled {
        cause: JobCancelCause,
        forced: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionFailure {
    Attempt {
        attempt: AttemptNumber,
        end: AttemptEnd,
    },
    SupervisorUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InfrastructureFailure {
    SupervisorLost { attempt: AttemptNumber },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExitCode(u32);

impl ExitCode {
    pub const SUCCESS: Self = Self(0);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Win32ErrorCode(u32);

impl Win32ErrorCode {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionEvent {
    LaunchSucceeded {
        attempt: AttemptNumber,
        at: BootInstant,
    },
    LaunchFailed {
        attempt: AttemptNumber,
        failure: LaunchFailure,
        at: BootInstant,
    },
    ProcessExited {
        attempt: AttemptNumber,
        exit_code: ExitCode,
        ran_for: Duration,
        at: BootInstant,
    },
    StopRequested {
        cause: StopCause,
        plan: StopPlan,
        at: BootInstant,
    },
    RestartDeadlineReached {
        deadline: RetryDeadline,
    },
    StopDeadlineReached {
        deadline: StopDeadline,
    },
    RestartPolicyChanged {
        policy: ExecutionRestartPolicy,
    },
    SuccessExitCodesChanged {
        codes: BTreeSet<ExitCode>,
    },
    SupervisorLost {
        at: BootInstant,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionEffect {
    LaunchAttempt { attempt: AttemptNumber },
    ScheduleRestart { retry_at: RetryDeadline },
    SendCtrlBreak,
    RunStopCommand,
    ScheduleStopDeadline { deadline: StopDeadline },
    TerminateJobObject,
    PublishTerminal { outcome: ExecutionOutcome },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionTransitionError {
    UnexpectedEvent,
    StaleAttempt,
    StaleDeadline,
    AttemptOverflow,
    RetryOverflow,
    DeadlineOverflow,
    PolicyKindMismatch,
}

impl Display for ExecutionTransitionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEvent => {
                formatter.write_str("event is invalid in the current execution phase")
            }
            Self::StaleAttempt => formatter.write_str("event refers to a stale process attempt"),
            Self::StaleDeadline => {
                formatter.write_str("event refers to a stale lifecycle deadline")
            }
            Self::AttemptOverflow => formatter.write_str("process attempt number overflowed"),
            Self::RetryOverflow => formatter.write_str("restart ordinal overflowed"),
            Self::DeadlineOverflow => {
                formatter.write_str("boot-relative lifecycle deadline overflowed")
            }
            Self::PolicyKindMismatch => {
                formatter.write_str("restart policy does not match the execution origin")
            }
        }
    }
}

impl std::error::Error for ExecutionTransitionError {}

impl ExecutionState {
    pub fn transition(
        self,
        event: ExecutionEvent,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        match event {
            ExecutionEvent::LaunchSucceeded { attempt, at: _ } => self.launch_succeeded(attempt),
            ExecutionEvent::LaunchFailed {
                attempt,
                failure,
                at,
            } => self.launch_failed(attempt, failure, at),
            ExecutionEvent::ProcessExited {
                attempt,
                exit_code,
                ran_for,
                at,
            } => self.process_exited(attempt, exit_code, ran_for, at),
            ExecutionEvent::StopRequested { cause, plan, at } => {
                self.stop_requested(cause, plan, at)
            }
            ExecutionEvent::RestartDeadlineReached { deadline } => {
                self.restart_deadline_reached(deadline)
            }
            ExecutionEvent::StopDeadlineReached { deadline } => {
                self.stop_deadline_reached(deadline)
            }
            ExecutionEvent::RestartPolicyChanged { policy } => self.restart_policy_changed(policy),
            ExecutionEvent::SuccessExitCodesChanged { codes } => {
                self.success_exit_codes_changed(codes)
            }
            ExecutionEvent::SupervisorLost { at } => self.supervisor_lost(at),
        }
    }

    fn launch_succeeded(
        mut self,
        attempt: AttemptNumber,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        match self.phase {
            ExecutionPhase::Launching { attempt: current } if current == attempt => {
                self.phase = ExecutionPhase::Running { attempt };
                Ok(Transition::without_effects(self))
            }
            ExecutionPhase::Launching { .. } => Err(ExecutionTransitionError::StaleAttempt),
            ExecutionPhase::Stopping {
                attempt: current,
                phase: StopPhase::AwaitingLaunch { deadline },
                cause,
                plan,
            } if current == attempt => {
                self.phase = ExecutionPhase::Stopping {
                    attempt,
                    phase: StopPhase::Graceful { deadline },
                    cause,
                    plan,
                };
                let effect = match plan {
                    StopPlan::CtrlBreak { .. } => ExecutionEffect::SendCtrlBreak,
                    StopPlan::Command { .. } => ExecutionEffect::RunStopCommand,
                    StopPlan::Kill => return Err(ExecutionTransitionError::UnexpectedEvent),
                };
                Ok(Transition::new(self, vec![effect]))
            }
            ExecutionPhase::Stopping {
                attempt: current, ..
            } if current != attempt => Err(ExecutionTransitionError::StaleAttempt),
            _ => Err(ExecutionTransitionError::UnexpectedEvent),
        }
    }

    fn launch_failed(
        self,
        attempt: AttemptNumber,
        failure: LaunchFailure,
        at: BootInstant,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        match self.phase {
            ExecutionPhase::Launching { attempt: current } if current == attempt => {
                self.finish_attempt(attempt, AttemptEnd::LaunchFailed { failure }, at)
            }
            ExecutionPhase::Launching { .. } => Err(ExecutionTransitionError::StaleAttempt),
            ExecutionPhase::Stopping {
                attempt: current,
                cause,
                ..
            } if current == attempt => Ok(self.finish_stop(cause, false)),
            ExecutionPhase::Stopping { .. } => Err(ExecutionTransitionError::StaleAttempt),
            _ => Err(ExecutionTransitionError::UnexpectedEvent),
        }
    }

    fn process_exited(
        self,
        attempt: AttemptNumber,
        exit_code: ExitCode,
        ran_for: Duration,
        at: BootInstant,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        match self.phase {
            ExecutionPhase::Running { attempt: current } if current == attempt => {
                self.finish_attempt(attempt, AttemptEnd::Exited { exit_code, ran_for }, at)
            }
            ExecutionPhase::Running { .. } => Err(ExecutionTransitionError::StaleAttempt),
            ExecutionPhase::Stopping {
                attempt: current,
                phase,
                cause,
                ..
            } if current == attempt => {
                Ok(self.finish_stop(cause, matches!(phase, StopPhase::Killing)))
            }
            ExecutionPhase::Stopping { .. } => Err(ExecutionTransitionError::StaleAttempt),
            _ => Err(ExecutionTransitionError::UnexpectedEvent),
        }
    }

    fn stop_requested(
        mut self,
        cause: StopCause,
        plan: StopPlan,
        at: BootInstant,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        self.validate_stop_cause(cause)?;
        match self.phase {
            ExecutionPhase::Launching { attempt } => match plan {
                StopPlan::Kill => {
                    self.phase = ExecutionPhase::Stopping {
                        attempt,
                        phase: StopPhase::Killing,
                        cause,
                        plan,
                    };
                    Ok(Transition::new(
                        self,
                        vec![ExecutionEffect::TerminateJobObject],
                    ))
                }
                StopPlan::CtrlBreak { timeout } | StopPlan::Command { timeout } => {
                    let deadline = self.stop_deadline(at, timeout)?;
                    self.phase = ExecutionPhase::Stopping {
                        attempt,
                        phase: StopPhase::AwaitingLaunch { deadline },
                        cause,
                        plan,
                    };
                    Ok(Transition::new(
                        self,
                        vec![ExecutionEffect::ScheduleStopDeadline { deadline }],
                    ))
                }
            },
            ExecutionPhase::Running { attempt } => match plan {
                StopPlan::Kill => {
                    self.phase = ExecutionPhase::Stopping {
                        attempt,
                        phase: StopPhase::Killing,
                        cause,
                        plan,
                    };
                    Ok(Transition::new(
                        self,
                        vec![ExecutionEffect::TerminateJobObject],
                    ))
                }
                StopPlan::CtrlBreak { timeout } | StopPlan::Command { timeout } => {
                    let deadline = self.stop_deadline(at, timeout)?;
                    self.phase = ExecutionPhase::Stopping {
                        attempt,
                        phase: StopPhase::Graceful { deadline },
                        cause,
                        plan,
                    };
                    let stop_effect = match plan {
                        StopPlan::CtrlBreak { .. } => ExecutionEffect::SendCtrlBreak,
                        StopPlan::Command { .. } => ExecutionEffect::RunStopCommand,
                        StopPlan::Kill => unreachable!("the kill plan is handled above"),
                    };
                    Ok(Transition::new(
                        self,
                        vec![
                            stop_effect,
                            ExecutionEffect::ScheduleStopDeadline { deadline },
                        ],
                    ))
                }
            },
            ExecutionPhase::RestartBackoff { .. } => Ok(self.finish_stop(cause, false)),
            ExecutionPhase::Stopping { .. } | ExecutionPhase::Terminal { .. } => {
                Ok(Transition::without_effects(self))
            }
        }
    }

    fn restart_deadline_reached(
        mut self,
        deadline: RetryDeadline,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        match self.phase {
            ExecutionPhase::RestartBackoff {
                next_attempt,
                retry_at,
                ..
            } if retry_at == deadline => {
                self.phase = ExecutionPhase::Launching {
                    attempt: next_attempt,
                };
                Ok(Transition::new(
                    self,
                    vec![ExecutionEffect::LaunchAttempt {
                        attempt: next_attempt,
                    }],
                ))
            }
            ExecutionPhase::RestartBackoff { .. } => Err(ExecutionTransitionError::StaleDeadline),
            _ => Err(ExecutionTransitionError::UnexpectedEvent),
        }
    }

    fn stop_deadline_reached(
        mut self,
        deadline: StopDeadline,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        match self.phase {
            ExecutionPhase::Stopping {
                attempt,
                phase:
                    StopPhase::AwaitingLaunch { deadline: current }
                    | StopPhase::Graceful { deadline: current },
                cause,
                plan,
            } if current == deadline => {
                self.phase = ExecutionPhase::Stopping {
                    attempt,
                    phase: StopPhase::Killing,
                    cause,
                    plan,
                };
                Ok(Transition::new(
                    self,
                    vec![ExecutionEffect::TerminateJobObject],
                ))
            }
            ExecutionPhase::Stopping {
                phase: StopPhase::AwaitingLaunch { .. } | StopPhase::Graceful { .. },
                ..
            } => Err(ExecutionTransitionError::StaleDeadline),
            ExecutionPhase::Stopping {
                phase: StopPhase::Killing,
                ..
            } => Ok(Transition::without_effects(self)),
            _ => Err(ExecutionTransitionError::UnexpectedEvent),
        }
    }

    fn restart_policy_changed(
        mut self,
        policy: ExecutionRestartPolicy,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        self.validate_policy_kind(policy)?;
        self.restart_policy = policy;
        self.reconsider_backoff()
    }

    fn success_exit_codes_changed(
        mut self,
        codes: BTreeSet<ExitCode>,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        self.success_exit_codes = codes;
        self.reconsider_backoff()
    }

    fn reconsider_backoff(
        self,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        let ExecutionPhase::RestartBackoff {
            previous_attempt,
            previous_end,
            retry,
            ..
        } = self.phase
        else {
            return Ok(Transition::without_effects(self));
        };

        if self.policy_permits(previous_end, retry)? {
            Ok(Transition::without_effects(self))
        } else {
            Ok(self.finish_terminal(previous_attempt, previous_end))
        }
    }

    fn supervisor_lost(
        self,
        at: BootInstant,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        match self.phase {
            ExecutionPhase::Launching { attempt } | ExecutionPhase::Running { attempt } => {
                if matches!(self.origin, ExecutionOrigin::Job(_)) {
                    Ok(self.terminal(ExecutionOutcome::OutcomeUnknown {
                        failure: InfrastructureFailure::SupervisorLost { attempt },
                    }))
                } else {
                    self.finish_attempt(attempt, AttemptEnd::SupervisorLost, at)
                }
            }
            ExecutionPhase::Stopping { cause, .. } => Ok(self.finish_stop(cause, true)),
            ExecutionPhase::RestartBackoff { .. } => Err(ExecutionTransitionError::UnexpectedEvent),
            ExecutionPhase::Terminal { .. } => Ok(Transition::without_effects(self)),
        }
    }

    fn finish_attempt(
        mut self,
        attempt: AttemptNumber,
        end: AttemptEnd,
        at: BootInstant,
    ) -> Result<Transition<Self, ExecutionEffect>, ExecutionTransitionError> {
        if let (
            ExecutionOrigin::ServiceIntent { .. },
            AttemptEnd::Exited { ran_for, .. },
            ExecutionRestartPolicy::Service(
                crate::restart::ServiceRestartPolicy::OnFailure(policy)
                | crate::restart::ServiceRestartPolicy::Always(policy),
            ),
        ) = (self.origin, end, self.restart_policy)
            && ran_for >= policy.reset_after().get()
        {
            self.last_retry = None;
        }

        let retry = match self.last_retry {
            Some(previous) => previous
                .next()
                .ok_or(ExecutionTransitionError::RetryOverflow)?,
            None => RetryOrdinal::FIRST,
        };

        if !self.policy_permits(end, retry)? {
            return Ok(self.finish_terminal(attempt, end));
        }

        let next_attempt = attempt
            .next()
            .ok_or(ExecutionTransitionError::AttemptOverflow)?;
        let delay = self.backoff()?.delay_for(retry);
        let retry_at = RetryDeadline::at(
            at.checked_add(delay)
                .ok_or(ExecutionTransitionError::DeadlineOverflow)?,
        );
        self.last_retry = Some(retry);
        self.phase = ExecutionPhase::RestartBackoff {
            previous_attempt: attempt,
            previous_end: end,
            next_attempt,
            retry,
            retry_at,
        };
        Ok(Transition::new(
            self,
            vec![ExecutionEffect::ScheduleRestart { retry_at }],
        ))
    }

    fn policy_permits(
        &self,
        end: AttemptEnd,
        retry: RetryOrdinal,
    ) -> Result<bool, ExecutionTransitionError> {
        use crate::restart::{JobRestartPolicy, ServiceRestartPolicy, ServiceRetryLimit};

        let failed = match end {
            AttemptEnd::Exited { exit_code, .. } => !self.success_exit_codes.contains(&exit_code),
            AttemptEnd::LaunchFailed { .. } | AttemptEnd::Killed | AttemptEnd::SupervisorLost => {
                true
            }
        };
        match (self.origin, self.restart_policy) {
            (ExecutionOrigin::ServiceIntent { .. }, ExecutionRestartPolicy::Service(policy)) => {
                let retry_policy = match policy {
                    ServiceRestartPolicy::Never => return Ok(false),
                    ServiceRestartPolicy::OnFailure(policy) if failed => policy,
                    ServiceRestartPolicy::OnFailure(_) => return Ok(false),
                    ServiceRestartPolicy::Always(policy) => policy,
                };
                Ok(match retry_policy.limit() {
                    ServiceRetryLimit::Finite(maximum) => retry.get() <= maximum.get(),
                    ServiceRetryLimit::Unlimited => true,
                })
            }
            (ExecutionOrigin::Job(_), ExecutionRestartPolicy::Job(policy)) => match policy {
                JobRestartPolicy::Never => Ok(false),
                JobRestartPolicy::OnFailure(policy) => {
                    Ok(failed && retry.get() <= policy.max_restarts().get())
                }
            },
            _ => Err(ExecutionTransitionError::PolicyKindMismatch),
        }
    }

    fn backoff(&self) -> Result<crate::restart::BackoffPolicy, ExecutionTransitionError> {
        use crate::restart::{JobRestartPolicy, ServiceRestartPolicy};

        match (self.origin, self.restart_policy) {
            (
                ExecutionOrigin::ServiceIntent { .. },
                ExecutionRestartPolicy::Service(
                    ServiceRestartPolicy::OnFailure(policy) | ServiceRestartPolicy::Always(policy),
                ),
            ) => Ok(policy.backoff()),
            (
                ExecutionOrigin::Job(_),
                ExecutionRestartPolicy::Job(JobRestartPolicy::OnFailure(policy)),
            ) => Ok(policy.backoff()),
            _ => Err(ExecutionTransitionError::PolicyKindMismatch),
        }
    }

    fn validate_policy_kind(
        &self,
        policy: ExecutionRestartPolicy,
    ) -> Result<(), ExecutionTransitionError> {
        if matches!(
            (self.origin, policy),
            (
                ExecutionOrigin::ServiceIntent { .. },
                ExecutionRestartPolicy::Service(_)
            ) | (ExecutionOrigin::Job(_), ExecutionRestartPolicy::Job(_))
        ) {
            Ok(())
        } else {
            Err(ExecutionTransitionError::PolicyKindMismatch)
        }
    }

    fn validate_stop_cause(&self, cause: StopCause) -> Result<(), ExecutionTransitionError> {
        if matches!(
            (self.origin, cause),
            (ExecutionOrigin::ServiceIntent { .. }, StopCause::Service(_))
                | (ExecutionOrigin::Job(_), StopCause::Job(_))
        ) {
            Ok(())
        } else {
            Err(ExecutionTransitionError::UnexpectedEvent)
        }
    }

    fn stop_deadline(
        &self,
        at: BootInstant,
        timeout: Duration,
    ) -> Result<StopDeadline, ExecutionTransitionError> {
        at.checked_add(timeout)
            .map(StopDeadline::at)
            .ok_or(ExecutionTransitionError::DeadlineOverflow)
    }

    fn finish_terminal(
        self,
        attempt: AttemptNumber,
        end: AttemptEnd,
    ) -> Transition<Self, ExecutionEffect> {
        let outcome = match end {
            AttemptEnd::Exited { exit_code, .. }
                if self.success_exit_codes.contains(&exit_code) =>
            {
                ExecutionOutcome::Completed { exit_code }
            }
            _ => ExecutionOutcome::Failed {
                failure: ExecutionFailure::Attempt { attempt, end },
            },
        };
        self.terminal(outcome)
    }

    fn finish_stop(self, cause: StopCause, forced: bool) -> Transition<Self, ExecutionEffect> {
        let outcome = match cause {
            StopCause::Service(cause) => ExecutionOutcome::Stopped { cause, forced },
            StopCause::Job(cause) => ExecutionOutcome::Cancelled { cause, forced },
        };
        self.terminal(outcome)
    }

    fn terminal(mut self, outcome: ExecutionOutcome) -> Transition<Self, ExecutionEffect> {
        self.phase = ExecutionPhase::Terminal { outcome };
        Transition::new(self, vec![ExecutionEffect::PublishTerminal { outcome }])
    }
}
