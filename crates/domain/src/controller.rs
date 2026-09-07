use crate::execution::{ExecutionOutcome, JobCancelCause, JobExecutionOrigin, ServiceStopCause};
use crate::ids::{
    ConfigGeneration, ExecutionId, IntentRevision, ManagerSessionId, ProcessDefinitionHash,
};
use crate::transition::Transition;
use std::fmt::{self, Display, Formatter};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Enablement {
    Disabled,
    Enabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefinitionState {
    Available {
        generation: ConfigGeneration,
        process_definition: ProcessDefinitionHash,
    },
    Missing,
}

impl DefinitionState {
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceState {
    pub definition: DefinitionState,
    pub enablement: Enablement,
    pub control: ServiceControl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceControl {
    Stopped(StoppedPhase),
    Running {
        revision: IntentRevision,
        phase: RunningPhase,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoppedPhase {
    Idle,
    Draining { execution_id: ExecutionId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunningPhase {
    NeedsExecution,
    Active {
        execution_id: ExecutionId,
    },
    Draining {
        execution_id: ExecutionId,
    },
    Blocked {
        execution_id: ExecutionId,
        outcome: ExecutionOutcome,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceControlEvent {
    Command(ServiceCommand),
    ExecutionAllocated {
        revision: IntentRevision,
        execution_id: ExecutionId,
    },
    ExecutionTerminated {
        execution_id: ExecutionId,
        outcome: ExecutionOutcome,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceCommand {
    Start { revision: IntentRevision },
    Restart { revision: IntentRevision },
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceControlEffect {
    AllocateExecution {
        revision: IntentRevision,
    },
    StopExecution {
        execution_id: ExecutionId,
        cause: ServiceStopCause,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobState {
    pub definition: DefinitionState,
    pub enablement: JobEnablement,
    pub activity: JobActivity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobEnablement {
    pub state: Enablement,
    pub last_trigger: SessionTrigger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTrigger {
    Never,
    Triggered(ManagerSessionId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobActivity {
    Idle,
    Active {
        execution_id: ExecutionId,
        after: AfterActive,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AfterActive {
    Nothing,
    Rerun,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobActivityEvent {
    StartRequested {
        execution_id: ExecutionId,
        origin: JobExecutionOrigin,
    },
    RerunRequested {
        execution_id_if_idle: ExecutionId,
    },
    CancelRequested,
    ExecutionTerminated {
        execution_id: ExecutionId,
        outcome: ExecutionOutcome,
        rerun: RerunDisposition,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RerunDisposition {
    NotRequested,
    Start { execution_id: ExecutionId },
    DefinitionMissing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobActivityEffect {
    StartExecution {
        execution_id: ExecutionId,
        origin: JobExecutionOrigin,
    },
    CancelExecution {
        execution_id: ExecutionId,
        cause: JobCancelCause,
    },
    RerunSkippedDefinitionMissing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerTransitionError {
    DefinitionMissing,
    StaleExecution,
    StaleIntentRevision,
}

impl Display for ControllerTransitionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefinitionMissing => formatter.write_str("workload definition is missing"),
            Self::StaleExecution => formatter.write_str("event refers to a stale execution"),
            Self::StaleIntentRevision => {
                formatter.write_str("event refers to a stale service intent revision")
            }
        }
    }
}

impl std::error::Error for ControllerTransitionError {}

impl ServiceState {
    pub fn transition(
        self,
        event: ServiceControlEvent,
    ) -> Result<Transition<Self, ServiceControlEffect>, ControllerTransitionError> {
        match event {
            ServiceControlEvent::Command(command) => self.command(command),
            ServiceControlEvent::ExecutionAllocated {
                revision,
                execution_id,
            } => self.execution_allocated(revision, execution_id),
            ServiceControlEvent::ExecutionTerminated {
                execution_id,
                outcome,
            } => self.execution_terminated(execution_id, outcome),
        }
    }

    fn command(
        mut self,
        command: ServiceCommand,
    ) -> Result<Transition<Self, ServiceControlEffect>, ControllerTransitionError> {
        match command {
            ServiceCommand::Start { revision } => {
                if !self.definition.is_available() {
                    return Err(ControllerTransitionError::DefinitionMissing);
                }
                let effects = match self.control {
                    ServiceControl::Stopped(StoppedPhase::Idle)
                    | ServiceControl::Running {
                        phase: RunningPhase::Blocked { .. },
                        ..
                    } => {
                        self.control = ServiceControl::Running {
                            revision,
                            phase: RunningPhase::NeedsExecution,
                        };
                        vec![ServiceControlEffect::AllocateExecution { revision }]
                    }
                    ServiceControl::Stopped(StoppedPhase::Draining { execution_id }) => {
                        self.control = ServiceControl::Running {
                            revision,
                            phase: RunningPhase::Draining { execution_id },
                        };
                        Vec::new()
                    }
                    ServiceControl::Running { .. } => Vec::new(),
                };
                Ok(Transition::new(self, effects))
            }
            ServiceCommand::Restart { revision } => {
                if !self.definition.is_available() {
                    return Err(ControllerTransitionError::DefinitionMissing);
                }
                let effects = match self.control {
                    ServiceControl::Stopped(StoppedPhase::Idle)
                    | ServiceControl::Running {
                        phase: RunningPhase::NeedsExecution | RunningPhase::Blocked { .. },
                        ..
                    } => {
                        self.control = ServiceControl::Running {
                            revision,
                            phase: RunningPhase::NeedsExecution,
                        };
                        vec![ServiceControlEffect::AllocateExecution { revision }]
                    }
                    ServiceControl::Stopped(StoppedPhase::Draining { execution_id })
                    | ServiceControl::Running {
                        phase: RunningPhase::Draining { execution_id },
                        ..
                    } => {
                        self.control = ServiceControl::Running {
                            revision,
                            phase: RunningPhase::Draining { execution_id },
                        };
                        Vec::new()
                    }
                    ServiceControl::Running {
                        phase: RunningPhase::Active { execution_id },
                        ..
                    } => {
                        self.control = ServiceControl::Running {
                            revision,
                            phase: RunningPhase::Draining { execution_id },
                        };
                        vec![ServiceControlEffect::StopExecution {
                            execution_id,
                            cause: ServiceStopCause::Restart,
                        }]
                    }
                };
                Ok(Transition::new(self, effects))
            }
            ServiceCommand::Stop => {
                let effects = match self.control {
                    ServiceControl::Stopped(_) => Vec::new(),
                    ServiceControl::Running {
                        phase:
                            RunningPhase::Active { execution_id }
                            | RunningPhase::Draining { execution_id },
                        ..
                    } => {
                        self.control =
                            ServiceControl::Stopped(StoppedPhase::Draining { execution_id });
                        vec![ServiceControlEffect::StopExecution {
                            execution_id,
                            cause: ServiceStopCause::Stop,
                        }]
                    }
                    ServiceControl::Running {
                        phase: RunningPhase::NeedsExecution | RunningPhase::Blocked { .. },
                        ..
                    } => {
                        self.control = ServiceControl::Stopped(StoppedPhase::Idle);
                        Vec::new()
                    }
                };
                Ok(Transition::new(self, effects))
            }
        }
    }

    fn execution_allocated(
        mut self,
        revision: IntentRevision,
        execution_id: ExecutionId,
    ) -> Result<Transition<Self, ServiceControlEffect>, ControllerTransitionError> {
        match self.control {
            ServiceControl::Running {
                revision: current,
                phase: RunningPhase::NeedsExecution,
            } if current == revision => {
                self.control = ServiceControl::Running {
                    revision,
                    phase: RunningPhase::Active { execution_id },
                };
                Ok(Transition::without_effects(self))
            }
            ServiceControl::Running {
                revision: current, ..
            } if current != revision => Err(ControllerTransitionError::StaleIntentRevision),
            _ => Err(ControllerTransitionError::StaleExecution),
        }
    }

    fn execution_terminated(
        mut self,
        execution_id: ExecutionId,
        outcome: ExecutionOutcome,
    ) -> Result<Transition<Self, ServiceControlEffect>, ControllerTransitionError> {
        let mut effects = Vec::new();
        self.control = match self.control {
            ServiceControl::Stopped(StoppedPhase::Draining {
                execution_id: current,
            }) if current == execution_id => ServiceControl::Stopped(StoppedPhase::Idle),
            ServiceControl::Running {
                revision,
                phase:
                    RunningPhase::Active {
                        execution_id: current,
                    },
            } if current == execution_id => ServiceControl::Running {
                revision,
                phase: RunningPhase::Blocked {
                    execution_id,
                    outcome,
                },
            },
            ServiceControl::Running {
                revision,
                phase:
                    RunningPhase::Draining {
                        execution_id: current,
                    },
            } if current == execution_id => {
                effects.push(ServiceControlEffect::AllocateExecution { revision });
                ServiceControl::Running {
                    revision,
                    phase: RunningPhase::NeedsExecution,
                }
            }
            _ => return Err(ControllerTransitionError::StaleExecution),
        };
        Ok(Transition::new(self, effects))
    }
}

impl JobState {
    pub fn transition(
        mut self,
        event: JobActivityEvent,
    ) -> Result<Transition<Self, JobActivityEffect>, ControllerTransitionError> {
        let effects = match event {
            JobActivityEvent::StartRequested {
                execution_id,
                origin,
            } => {
                if !self.definition.is_available() {
                    return Err(ControllerTransitionError::DefinitionMissing);
                }
                match self.activity {
                    JobActivity::Idle => {
                        self.activity = JobActivity::Active {
                            execution_id,
                            after: AfterActive::Nothing,
                        };
                        vec![JobActivityEffect::StartExecution {
                            execution_id,
                            origin,
                        }]
                    }
                    JobActivity::Active { .. } => Vec::new(),
                }
            }
            JobActivityEvent::RerunRequested {
                execution_id_if_idle,
            } => {
                if !self.definition.is_available() {
                    return Err(ControllerTransitionError::DefinitionMissing);
                }
                match self.activity {
                    JobActivity::Idle => {
                        self.activity = JobActivity::Active {
                            execution_id: execution_id_if_idle,
                            after: AfterActive::Nothing,
                        };
                        vec![JobActivityEffect::StartExecution {
                            execution_id: execution_id_if_idle,
                            origin: JobExecutionOrigin::Manual,
                        }]
                    }
                    JobActivity::Active {
                        execution_id,
                        after: AfterActive::Nothing,
                    } => {
                        self.activity = JobActivity::Active {
                            execution_id,
                            after: AfterActive::Rerun,
                        };
                        vec![JobActivityEffect::CancelExecution {
                            execution_id,
                            cause: JobCancelCause::Rerun,
                        }]
                    }
                    JobActivity::Active {
                        after: AfterActive::Rerun,
                        ..
                    } => Vec::new(),
                }
            }
            JobActivityEvent::CancelRequested => match self.activity {
                JobActivity::Idle => Vec::new(),
                JobActivity::Active { execution_id, .. } => {
                    self.activity = JobActivity::Active {
                        execution_id,
                        after: AfterActive::Nothing,
                    };
                    vec![JobActivityEffect::CancelExecution {
                        execution_id,
                        cause: JobCancelCause::Cancel,
                    }]
                }
            },
            JobActivityEvent::ExecutionTerminated {
                execution_id,
                outcome: _,
                rerun,
            } => match self.activity {
                JobActivity::Active {
                    execution_id: current,
                    after,
                } if current == execution_id => match (after, rerun) {
                    (AfterActive::Nothing, RerunDisposition::NotRequested)
                    | (AfterActive::Rerun, RerunDisposition::DefinitionMissing) => {
                        self.activity = JobActivity::Idle;
                        if matches!(rerun, RerunDisposition::DefinitionMissing) {
                            vec![JobActivityEffect::RerunSkippedDefinitionMissing]
                        } else {
                            Vec::new()
                        }
                    }
                    (AfterActive::Rerun, RerunDisposition::Start { execution_id: next }) => {
                        self.activity = JobActivity::Active {
                            execution_id: next,
                            after: AfterActive::Nothing,
                        };
                        vec![JobActivityEffect::StartExecution {
                            execution_id: next,
                            origin: JobExecutionOrigin::Rerun {
                                previous_execution_id: execution_id,
                            },
                        }]
                    }
                    _ => return Err(ControllerTransitionError::StaleExecution),
                },
                _ => return Err(ControllerTransitionError::StaleExecution),
            },
        };
        Ok(Transition::new(self, effects))
    }
}
