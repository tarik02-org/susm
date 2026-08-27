use std::num::NonZeroU32;
use std::time::Duration;

use crate::ids::{ExecutionId, ObservationSequence, SupervisorId, SupervisorIncarnation};
use crate::restart::{BackoffPolicy, RetryOrdinal};
use crate::time::{
    BootInstant, DiscoveryDeadline, HandshakeDeadline, NonZeroDuration, RetryDeadline,
};
use crate::transition::Transition;
use std::fmt::{self, Display, Formatter};

pub const DEFAULT_SUPERVISOR_MAX_FAILURES: u32 = 8;
pub const DEFAULT_SUPERVISOR_BACKOFF_INITIAL: Duration = Duration::from_millis(100);
pub const DEFAULT_SUPERVISOR_BACKOFF_MULTIPLIER: u32 = 2;
pub const DEFAULT_SUPERVISOR_BACKOFF_MAXIMUM: Duration = Duration::from_secs(5);
pub const DEFAULT_SUPERVISOR_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_SUPERVISOR_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SupervisorProcessIdentity {
    supervisor_id: SupervisorId,
    incarnation: SupervisorIncarnation,
}

impl SupervisorProcessIdentity {
    pub const fn new(supervisor_id: SupervisorId, incarnation: SupervisorIncarnation) -> Self {
        Self {
            supervisor_id,
            incarnation,
        }
    }

    pub const fn supervisor_id(self) -> SupervisorId {
        self.supervisor_id
    }

    pub const fn incarnation(self) -> SupervisorIncarnation {
        self.incarnation
    }

    pub fn next_incarnation(self) -> Option<Self> {
        self.incarnation
            .next()
            .map(|incarnation| Self::new(self.supervisor_id, incarnation))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SupervisorFailureCount(u32);

impl SupervisorFailureCount {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    pub fn after_failure(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }

    pub fn retry_ordinal(self) -> Option<RetryOrdinal> {
        RetryOrdinal::new(self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisorLaunchPolicy {
    max_failures: NonZeroU32,
    backoff: BackoffPolicy,
    handshake_timeout: NonZeroDuration,
    discovery_timeout: NonZeroDuration,
}

impl SupervisorLaunchPolicy {
    pub const fn new(
        max_failures: NonZeroU32,
        backoff: BackoffPolicy,
        handshake_timeout: NonZeroDuration,
        discovery_timeout: NonZeroDuration,
    ) -> Self {
        Self {
            max_failures,
            backoff,
            handshake_timeout,
            discovery_timeout,
        }
    }

    pub const fn max_failures(self) -> NonZeroU32 {
        self.max_failures
    }

    pub const fn backoff(self) -> BackoffPolicy {
        self.backoff
    }

    pub const fn handshake_timeout(self) -> NonZeroDuration {
        self.handshake_timeout
    }

    pub const fn discovery_timeout(self) -> NonZeroDuration {
        self.discovery_timeout
    }

    pub const fn permits_retry_after(self, failures: SupervisorFailureCount) -> bool {
        failures.get() < self.max_failures.get()
    }
}

impl Default for SupervisorLaunchPolicy {
    fn default() -> Self {
        let backoff = BackoffPolicy::new(
            NonZeroDuration::new(DEFAULT_SUPERVISOR_BACKOFF_INITIAL)
                .expect("default supervisor initial backoff is non-zero"),
            NonZeroU32::new(DEFAULT_SUPERVISOR_BACKOFF_MULTIPLIER)
                .expect("default supervisor backoff multiplier is non-zero"),
            NonZeroDuration::new(DEFAULT_SUPERVISOR_BACKOFF_MAXIMUM)
                .expect("default supervisor maximum backoff is non-zero"),
        )
        .expect("default supervisor maximum backoff is not below its initial delay");

        Self::new(
            NonZeroU32::new(DEFAULT_SUPERVISOR_MAX_FAILURES)
                .expect("default supervisor failure limit is non-zero"),
            backoff,
            NonZeroDuration::new(DEFAULT_SUPERVISOR_HANDSHAKE_TIMEOUT)
                .expect("default supervisor handshake timeout is non-zero"),
            NonZeroDuration::new(DEFAULT_SUPERVISOR_DISCOVERY_TIMEOUT)
                .expect("default supervisor discovery timeout is non-zero"),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorBinding {
    Discovering {
        process: SupervisorProcessIdentity,
        failures: SupervisorFailureCount,
        deadline: DiscoveryDeadline,
    },
    LaunchPending {
        process: SupervisorProcessIdentity,
        failures: SupervisorFailureCount,
    },
    AwaitingHandshake {
        process: SupervisorProcessIdentity,
        failures: SupervisorFailureCount,
        deadline: HandshakeDeadline,
    },
    Attached {
        process: SupervisorProcessIdentity,
        failures: SupervisorFailureCount,
        last_sequence: ObservationSequence,
    },
    RestartBackoff {
        next_process: SupervisorProcessIdentity,
        failures: SupervisorFailureCount,
        retry_at: RetryDeadline,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorBindingEvent {
    DiscoveryAttached {
        process: SupervisorProcessIdentity,
        last_sequence: ObservationSequence,
    },
    DiscoveryFailed {
        process: SupervisorProcessIdentity,
        at: BootInstant,
        failure: SupervisorFailure,
    },
    LaunchSucceeded {
        process: SupervisorProcessIdentity,
        at: BootInstant,
    },
    LaunchFailed {
        process: SupervisorProcessIdentity,
        at: BootInstant,
    },
    HandshakeAccepted {
        process: SupervisorProcessIdentity,
        last_sequence: ObservationSequence,
    },
    HandshakeFailed {
        process: SupervisorProcessIdentity,
        at: BootInstant,
        failure: SupervisorFailure,
    },
    SupervisorExited {
        process: SupervisorProcessIdentity,
        at: BootInstant,
    },
    RetryDeadlineReached {
        process: SupervisorProcessIdentity,
        deadline: RetryDeadline,
    },
    ExecutionTerminated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorFailure {
    ProcessMissing,
    ProcessExited,
    HandshakeTimedOut,
    HandshakeRejected(HandshakeRejection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandshakeRejection {
    AuthenticationFailed,
    WrongManagerSession,
    WrongExecution,
    WrongSupervisor,
    WrongExecutionConfig,
    IncompatibleProtocol,
    StaleObservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorBindingEffect {
    LaunchSupervisor {
        process: SupervisorProcessIdentity,
    },
    ArmDiscoveryDeadline {
        deadline: DiscoveryDeadline,
    },
    ArmHandshakeDeadline {
        deadline: HandshakeDeadline,
    },
    ArmRetryDeadline {
        deadline: RetryDeadline,
    },
    AdoptSupervisor {
        process: SupervisorProcessIdentity,
        execution_id: ExecutionId,
        from_sequence: ObservationSequence,
    },
    RequestSupervisorShutdown {
        process: SupervisorProcessIdentity,
    },
    ReportSupervisorUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorBindingError {
    UnexpectedEvent,
    StaleProcess,
    StaleDeadline,
    FailureCountOverflow,
    IncarnationOverflow,
    DeadlineOverflow,
}

impl Display for SupervisorBindingError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEvent => {
                formatter.write_str("event is invalid in the current supervisor binding phase")
            }
            Self::StaleProcess => formatter.write_str("event refers to a stale supervisor process"),
            Self::StaleDeadline => {
                formatter.write_str("event refers to a stale supervisor deadline")
            }
            Self::FailureCountOverflow => {
                formatter.write_str("supervisor failure count overflowed")
            }
            Self::IncarnationOverflow => formatter.write_str("supervisor incarnation overflowed"),
            Self::DeadlineOverflow => {
                formatter.write_str("boot-relative supervisor deadline overflowed")
            }
        }
    }
}

impl std::error::Error for SupervisorBindingError {}

impl SupervisorBinding {
    pub fn transition(
        self,
        event: SupervisorBindingEvent,
        execution_id: ExecutionId,
        policy: SupervisorLaunchPolicy,
    ) -> Result<Transition<Option<Self>, SupervisorBindingEffect>, SupervisorBindingError> {
        match event {
            SupervisorBindingEvent::DiscoveryAttached {
                process,
                last_sequence,
            } => self.discovery_attached(process, execution_id, last_sequence),
            SupervisorBindingEvent::DiscoveryFailed {
                process,
                at,
                failure: _,
            }
            | SupervisorBindingEvent::HandshakeFailed {
                process,
                at,
                failure: _,
            }
            | SupervisorBindingEvent::SupervisorExited { process, at } => {
                self.process_failed(process, at, policy)
            }
            SupervisorBindingEvent::LaunchSucceeded { process, at } => {
                self.launch_succeeded(process, at, policy)
            }
            SupervisorBindingEvent::LaunchFailed { process, at } => {
                self.process_failed(process, at, policy)
            }
            SupervisorBindingEvent::HandshakeAccepted {
                process,
                last_sequence,
            } => self.handshake_accepted(process, last_sequence),
            SupervisorBindingEvent::RetryDeadlineReached { process, deadline } => {
                self.retry_deadline_reached(process, deadline)
            }
            SupervisorBindingEvent::ExecutionTerminated => Ok(self.terminate()),
        }
    }

    fn discovery_attached(
        self,
        process: SupervisorProcessIdentity,
        execution_id: ExecutionId,
        last_sequence: ObservationSequence,
    ) -> Result<Transition<Option<Self>, SupervisorBindingEffect>, SupervisorBindingError> {
        let SupervisorBinding::Discovering {
            process: current,
            failures,
            ..
        } = self
        else {
            return Err(SupervisorBindingError::UnexpectedEvent);
        };
        if current != process {
            return Err(SupervisorBindingError::StaleProcess);
        }

        Ok(Transition::new(
            Some(SupervisorBinding::Attached {
                process,
                failures,
                last_sequence,
            }),
            vec![SupervisorBindingEffect::AdoptSupervisor {
                process,
                execution_id,
                from_sequence: last_sequence,
            }],
        ))
    }

    fn launch_succeeded(
        self,
        process: SupervisorProcessIdentity,
        at: BootInstant,
        policy: SupervisorLaunchPolicy,
    ) -> Result<Transition<Option<Self>, SupervisorBindingEffect>, SupervisorBindingError> {
        let SupervisorBinding::LaunchPending {
            process: current,
            failures,
        } = self
        else {
            return Err(SupervisorBindingError::UnexpectedEvent);
        };
        if current != process {
            return Err(SupervisorBindingError::StaleProcess);
        }
        let deadline = HandshakeDeadline::at(
            at.checked_add(policy.handshake_timeout().get())
                .ok_or(SupervisorBindingError::DeadlineOverflow)?,
        );

        Ok(Transition::new(
            Some(SupervisorBinding::AwaitingHandshake {
                process,
                failures,
                deadline,
            }),
            vec![SupervisorBindingEffect::ArmHandshakeDeadline { deadline }],
        ))
    }

    fn handshake_accepted(
        self,
        process: SupervisorProcessIdentity,
        last_sequence: ObservationSequence,
    ) -> Result<Transition<Option<Self>, SupervisorBindingEffect>, SupervisorBindingError> {
        let SupervisorBinding::AwaitingHandshake {
            process: current,
            failures,
            ..
        } = self
        else {
            return Err(SupervisorBindingError::UnexpectedEvent);
        };
        if current != process {
            return Err(SupervisorBindingError::StaleProcess);
        }

        Ok(Transition::without_effects(Some(
            SupervisorBinding::Attached {
                process,
                failures,
                last_sequence,
            },
        )))
    }

    fn process_failed(
        self,
        process: SupervisorProcessIdentity,
        at: BootInstant,
        policy: SupervisorLaunchPolicy,
    ) -> Result<Transition<Option<Self>, SupervisorBindingEffect>, SupervisorBindingError> {
        let (current, failures) = match self {
            SupervisorBinding::Discovering {
                process, failures, ..
            }
            | SupervisorBinding::LaunchPending { process, failures }
            | SupervisorBinding::AwaitingHandshake {
                process, failures, ..
            }
            | SupervisorBinding::Attached {
                process, failures, ..
            } => (process, failures),
            SupervisorBinding::RestartBackoff { .. } => {
                return Err(SupervisorBindingError::UnexpectedEvent);
            }
        };
        if current != process {
            return Err(SupervisorBindingError::StaleProcess);
        }
        let failures = failures
            .after_failure()
            .ok_or(SupervisorBindingError::FailureCountOverflow)?;
        if !policy.permits_retry_after(failures) {
            return Ok(Transition::new(
                None,
                vec![SupervisorBindingEffect::ReportSupervisorUnavailable],
            ));
        }
        let next_process = process
            .next_incarnation()
            .ok_or(SupervisorBindingError::IncarnationOverflow)?;
        let retry = failures
            .retry_ordinal()
            .ok_or(SupervisorBindingError::FailureCountOverflow)?;
        let retry_at = RetryDeadline::at(
            at.checked_add(policy.backoff().delay_for(retry))
                .ok_or(SupervisorBindingError::DeadlineOverflow)?,
        );
        Ok(Transition::new(
            Some(SupervisorBinding::RestartBackoff {
                next_process,
                failures,
                retry_at,
            }),
            vec![SupervisorBindingEffect::ArmRetryDeadline { deadline: retry_at }],
        ))
    }

    fn retry_deadline_reached(
        self,
        process: SupervisorProcessIdentity,
        deadline: RetryDeadline,
    ) -> Result<Transition<Option<Self>, SupervisorBindingEffect>, SupervisorBindingError> {
        let SupervisorBinding::RestartBackoff {
            next_process,
            failures,
            retry_at,
        } = self
        else {
            return Err(SupervisorBindingError::UnexpectedEvent);
        };
        if next_process != process {
            return Err(SupervisorBindingError::StaleProcess);
        }
        if retry_at != deadline {
            return Err(SupervisorBindingError::StaleDeadline);
        }

        Ok(Transition::new(
            Some(SupervisorBinding::LaunchPending { process, failures }),
            vec![SupervisorBindingEffect::LaunchSupervisor { process }],
        ))
    }

    fn terminate(self) -> Transition<Option<Self>, SupervisorBindingEffect> {
        let process = match self {
            SupervisorBinding::Discovering { process, .. }
            | SupervisorBinding::AwaitingHandshake { process, .. }
            | SupervisorBinding::Attached { process, .. } => Some(process),
            SupervisorBinding::LaunchPending { .. } | SupervisorBinding::RestartBackoff { .. } => {
                None
            }
        };
        let effects = process
            .map(|process| SupervisorBindingEffect::RequestSupervisorShutdown { process })
            .into_iter()
            .collect();
        Transition::new(None, effects)
    }
}
