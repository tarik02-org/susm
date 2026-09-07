use std::fmt::{self, Display, Formatter};
use std::num::{NonZeroU32, NonZeroU64};
use std::str::FromStr;

use uuid::Uuid;

pub const MAX_WORKLOAD_ID_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkloadId(Box<str>);

impl WorkloadId {
    pub fn parse(value: impl Into<Box<str>>) -> Result<Self, InvalidWorkloadId> {
        let value = value.into();
        let bytes = value.as_bytes();

        let Some(first) = bytes.first() else {
            return Err(InvalidWorkloadId::Empty);
        };
        let last = bytes.last().expect("a non-empty ID has a last byte");

        if bytes.len() > MAX_WORKLOAD_ID_BYTES {
            return Err(InvalidWorkloadId::TooLong);
        }

        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return Err(InvalidWorkloadId::InvalidBoundary);
        }
        if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
            return Err(InvalidWorkloadId::InvalidBoundary);
        }

        if let Some(index) = bytes.iter().position(|byte| {
            !byte.is_ascii_lowercase()
                && !byte.is_ascii_digit()
                && !matches!(byte, b'-' | b'_' | b'.')
        }) {
            return Err(InvalidWorkloadId::InvalidCharacter { index });
        }

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for WorkloadId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Display for WorkloadId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for WorkloadId {
    type Err = InvalidWorkloadId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidWorkloadId {
    Empty,
    TooLong,
    InvalidBoundary,
    InvalidCharacter { index: usize },
}

impl Display for InvalidWorkloadId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("workload ID is empty"),
            Self::TooLong => write!(
                formatter,
                "workload ID exceeds {MAX_WORKLOAD_ID_BYTES} ASCII bytes"
            ),
            Self::InvalidBoundary => {
                formatter.write_str("workload ID must start and end with [a-z0-9]")
            }
            Self::InvalidCharacter { index } => write!(
                formatter,
                "workload ID contains an invalid character at byte {index}"
            ),
        }
    }
}

impl std::error::Error for InvalidWorkloadId {}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ManagerSessionId(Uuid);

impl ManagerSessionId {
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Display for ManagerSessionId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExecutionId(Uuid);

impl ExecutionId {
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Display for ExecutionId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SupervisorId(Uuid);

impl SupervisorId {
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Display for SupervisorId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SupervisorIncarnation(NonZeroU32);

impl SupervisorIncarnation {
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IntentRevision(NonZeroU64);

impl IntentRevision {
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    pub fn next(self) -> Option<Self> {
        self.get().checked_add(1).and_then(Self::new)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AttemptNumber(NonZeroU32);

impl AttemptNumber {
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObservationSequence(u64);

impl ObservationSequence {
    pub const INITIAL: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn next(self) -> Option<Self> {
        self.get().checked_add(1).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ConfigGeneration([u8; 32]);

impl ConfigGeneration {
    pub const fn from_sha256(value: [u8; 32]) -> Self {
        Self(value)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProcessDefinitionHash([u8; 32]);

impl ProcessDefinitionHash {
    pub const fn from_sha256(value: [u8; 32]) -> Self {
        Self(value)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExecutionConfigHash([u8; 32]);

impl ExecutionConfigHash {
    pub const fn from_sha256(value: [u8; 32]) -> Self {
        Self(value)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
