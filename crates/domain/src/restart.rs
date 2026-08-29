use std::fmt::{self, Display, Formatter};
use std::num::NonZeroU32;
use std::time::Duration;

use crate::time::NonZeroDuration;

pub const DEFAULT_SERVICE_MAX_RESTARTS: u32 = 10;
pub const DEFAULT_SERVICE_RESET_AFTER: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_millis(250);
pub const DEFAULT_BACKOFF_MULTIPLIER: u32 = 2;
pub const DEFAULT_BACKOFF_MAXIMUM: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RetryOrdinal(NonZeroU32);

impl RetryOrdinal {
    pub const FIRST: Self = Self(NonZeroU32::MIN);

    pub const fn new(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }

    pub fn next(self) -> Option<Self> {
        self.get().checked_add(1).and_then(Self::new)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BackoffPolicy {
    initial: NonZeroDuration,
    multiplier: NonZeroU32,
    maximum: NonZeroDuration,
}

impl BackoffPolicy {
    pub fn new(
        initial: NonZeroDuration,
        multiplier: NonZeroU32,
        maximum: NonZeroDuration,
    ) -> Result<Self, InvalidBackoffPolicy> {
        if maximum < initial {
            return Err(InvalidBackoffPolicy::MaximumLessThanInitial);
        }

        Ok(Self {
            initial,
            multiplier,
            maximum,
        })
    }

    pub const fn initial(self) -> NonZeroDuration {
        self.initial
    }

    pub const fn multiplier(self) -> NonZeroU32 {
        self.multiplier
    }

    pub const fn maximum(self) -> NonZeroDuration {
        self.maximum
    }

    pub fn delay_for(self, retry: RetryOrdinal) -> Duration {
        let maximum = self.maximum.get();
        let multiplier = self.multiplier.get();
        let mut delay = self.initial.get();

        if multiplier == 1 || delay == maximum {
            return delay;
        }

        for _ in 1..retry.get() {
            delay = match delay.checked_mul(multiplier) {
                Some(next) if next < maximum => next,
                _ => return maximum,
            };
        }

        delay
    }
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self::new(
            NonZeroDuration::new(DEFAULT_BACKOFF_INITIAL)
                .expect("default initial backoff is non-zero"),
            NonZeroU32::new(DEFAULT_BACKOFF_MULTIPLIER)
                .expect("default backoff multiplier is non-zero"),
            NonZeroDuration::new(DEFAULT_BACKOFF_MAXIMUM)
                .expect("default maximum backoff is non-zero"),
        )
        .expect("default maximum backoff is not below its initial delay")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidBackoffPolicy {
    MaximumLessThanInitial,
}

impl Display for InvalidBackoffPolicy {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaximumLessThanInitial => {
                formatter.write_str("maximum backoff is less than initial backoff")
            }
        }
    }
}

impl std::error::Error for InvalidBackoffPolicy {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ServiceRetryLimit {
    Finite(NonZeroU32),
    Unlimited,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ServiceRetryPolicy {
    limit: ServiceRetryLimit,
    reset_after: NonZeroDuration,
    backoff: BackoffPolicy,
}

impl ServiceRetryPolicy {
    pub const fn new(
        limit: ServiceRetryLimit,
        reset_after: NonZeroDuration,
        backoff: BackoffPolicy,
    ) -> Self {
        Self {
            limit,
            reset_after,
            backoff,
        }
    }

    pub const fn limit(self) -> ServiceRetryLimit {
        self.limit
    }

    pub const fn reset_after(self) -> NonZeroDuration {
        self.reset_after
    }

    pub const fn backoff(self) -> BackoffPolicy {
        self.backoff
    }
}

impl Default for ServiceRetryPolicy {
    fn default() -> Self {
        Self::new(
            ServiceRetryLimit::Finite(
                NonZeroU32::new(DEFAULT_SERVICE_MAX_RESTARTS)
                    .expect("default service restart limit is non-zero"),
            ),
            NonZeroDuration::new(DEFAULT_SERVICE_RESET_AFTER)
                .expect("default service reset window is non-zero"),
            BackoffPolicy::default(),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ServiceRestartPolicy {
    Never,
    OnFailure(ServiceRetryPolicy),
    Always(ServiceRetryPolicy),
}

impl Default for ServiceRestartPolicy {
    fn default() -> Self {
        Self::OnFailure(ServiceRetryPolicy::default())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct JobRetryPolicy {
    max_restarts: NonZeroU32,
    backoff: BackoffPolicy,
}

impl JobRetryPolicy {
    pub const fn new(max_restarts: NonZeroU32, backoff: BackoffPolicy) -> Self {
        Self {
            max_restarts,
            backoff,
        }
    }

    pub const fn max_restarts(self) -> NonZeroU32 {
        self.max_restarts
    }

    pub const fn backoff(self) -> BackoffPolicy {
        self.backoff
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum JobRestartPolicy {
    #[default]
    Never,
    OnFailure(JobRetryPolicy),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExecutionRestartPolicy {
    Service(ServiceRestartPolicy),
    Job(JobRestartPolicy),
}
