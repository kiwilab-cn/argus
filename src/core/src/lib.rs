use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Health {
    Healthy,
    Unhealthy,
}

impl Health {
    #[must_use]
    pub const fn is_healthy(self) -> bool {
        matches!(self, Self::Healthy)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckOutcome {
    pub health: Health,
    pub summary: String,
    pub latency: Duration,
}

impl CheckOutcome {
    #[must_use]
    pub fn healthy(summary: impl Into<String>, latency: Duration) -> Self {
        Self {
            health: Health::Healthy,
            summary: summary.into(),
            latency,
        }
    }

    #[must_use]
    pub fn unhealthy(summary: impl Into<String>, latency: Duration) -> Self {
        Self {
            health: Health::Unhealthy,
            summary: summary.into(),
            latency,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlertKind {
    Firing,
    Recovered,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlertEvent {
    pub monitor: String,
    pub kind: AlertKind,
    pub summary: String,
    pub consecutive_failures: u32,
    pub incident_duration: Option<Duration>,
}
