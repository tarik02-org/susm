use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NonZeroDuration(Duration);

impl NonZeroDuration {
    pub fn new(value: Duration) -> Option<Self> {
        (!value.is_zero()).then_some(Self(value))
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BootInstant(u64);

impl BootInstant {
    pub const fn from_interrupt_time_100ns(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_interrupt_time_100ns(self) -> u64 {
        self.0
    }

    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        let whole_ticks = duration.as_secs().checked_mul(10_000_000)?;
        let fractional_ticks = u64::from(duration.subsec_nanos()).div_ceil(100);
        let duration_ticks = whole_ticks.checked_add(fractional_ticks)?;
        self.0.checked_add(duration_ticks).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RetryDeadline(BootInstant);

impl RetryDeadline {
    pub const fn at(value: BootInstant) -> Self {
        Self(value)
    }

    pub const fn instant(self) -> BootInstant {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiscoveryDeadline(BootInstant);

impl DiscoveryDeadline {
    pub const fn at(value: BootInstant) -> Self {
        Self(value)
    }

    pub const fn instant(self) -> BootInstant {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HandshakeDeadline(BootInstant);

impl HandshakeDeadline {
    pub const fn at(value: BootInstant) -> Self {
        Self(value)
    }

    pub const fn instant(self) -> BootInstant {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StopDeadline(BootInstant);

impl StopDeadline {
    pub const fn at(value: BootInstant) -> Self {
        Self(value)
    }

    pub const fn instant(self) -> BootInstant {
        self.0
    }
}
