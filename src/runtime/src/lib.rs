use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use argus_core::{AlertEvent, AlertKind, CheckOutcome, Health};
use async_trait::async_trait;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info, warn};

pub type NotifyError = Box<dyn Error + Send + Sync>;

#[async_trait]
pub trait MonitorPlugin: Send + Sync {
    fn name(&self) -> &str;

    fn interval(&self) -> Duration;

    async fn check(&self) -> CheckOutcome;
}

#[async_trait]
pub trait Notifier: Send + Sync {
    fn name(&self) -> &str;

    async fn notify(&self, event: &AlertEvent) -> Result<(), NotifyError>;
}

#[derive(Clone, Copy, Debug)]
pub struct AlertPolicy {
    pub failure_threshold: u32,
    pub recovery_threshold: u32,
}

impl AlertPolicy {
    #[must_use]
    pub const fn normalized(self) -> Self {
        Self {
            failure_threshold: if self.failure_threshold == 0 {
                1
            } else {
                self.failure_threshold
            },
            recovery_threshold: if self.recovery_threshold == 0 {
                1
            } else {
                self.recovery_threshold
            },
        }
    }
}

pub struct Runner {
    monitors: Vec<Arc<dyn MonitorPlugin>>,
    notifiers: Vec<Arc<dyn Notifier>>,
    policy: AlertPolicy,
}

impl Runner {
    #[must_use]
    pub fn new(policy: AlertPolicy) -> Self {
        Self {
            monitors: Vec::new(),
            notifiers: Vec::new(),
            policy: policy.normalized(),
        }
    }

    pub fn add_monitor(&mut self, monitor: Arc<dyn MonitorPlugin>) {
        self.monitors.push(monitor);
    }

    pub fn add_notifier(&mut self, notifier: Arc<dyn Notifier>) {
        self.notifiers.push(notifier);
    }

    #[must_use]
    pub fn monitor_count(&self) -> usize {
        self.monitors.len()
    }

    pub async fn run_once(&self) -> Vec<(String, CheckOutcome)> {
        let mut results = Vec::with_capacity(self.monitors.len());
        for monitor in &self.monitors {
            let outcome = monitor.check().await;
            log_outcome(monitor.name(), &outcome);
            results.push((monitor.name().to_owned(), outcome));
        }
        results
    }

    pub async fn run_until(self, shutdown: watch::Receiver<bool>) {
        let mut tasks = JoinSet::new();
        for monitor in self.monitors {
            let notifiers = self.notifiers.clone();
            let policy = self.policy;
            let monitor_shutdown = shutdown.clone();
            tasks.spawn(run_monitor(monitor, notifiers, policy, monitor_shutdown));
        }

        while let Some(result) = tasks.join_next().await {
            if let Err(join_error) = result {
                error!(error = %join_error, "monitor task terminated unexpectedly");
            }
        }
    }
}

async fn run_monitor(
    monitor: Arc<dyn MonitorPlugin>,
    notifiers: Vec<Arc<dyn Notifier>>,
    policy: AlertPolicy,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(monitor.interval());
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut tracker = IncidentTracker::new(policy);

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    info!(monitor = monitor.name(), "monitor stopped");
                    return;
                }
            }
            _ = ticker.tick() => {
                let outcome = monitor.check().await;
                log_outcome(monitor.name(), &outcome);
                if let Some(event) = tracker.observe(monitor.name(), &outcome, SystemTime::now())
                    && notify_all(&notifiers, &event).await
                {
                    tracker.mark_delivered(event.kind);
                }
            }
        }
    }
}

async fn notify_all(notifiers: &[Arc<dyn Notifier>], event: &AlertEvent) -> bool {
    if notifiers.is_empty() {
        error!(monitor = event.monitor, "alert has no configured notifier");
        return false;
    }

    let mut delivered = true;
    for notifier in notifiers {
        if let Err(notify_error) = notifier.notify(event).await {
            delivered = false;
            error!(
                notifier = notifier.name(),
                monitor = event.monitor,
                error = %notify_error,
                "failed to deliver alert"
            );
        }
    }
    delivered
}

fn log_outcome(monitor: &str, outcome: &CheckOutcome) {
    let latency_ms = outcome.latency.as_millis();
    match outcome.health {
        Health::Healthy => info!(
            monitor,
            latency_ms,
            summary = outcome.summary,
            "check healthy"
        ),
        Health::Unhealthy => {
            warn!(
                monitor,
                latency_ms,
                summary = outcome.summary,
                "check unhealthy"
            );
        }
    }
}

#[derive(Debug)]
struct IncidentTracker {
    policy: AlertPolicy,
    consecutive_failures: u32,
    consecutive_successes: u32,
    incident_started: Option<SystemTime>,
    incident_open: bool,
}

impl IncidentTracker {
    fn new(policy: AlertPolicy) -> Self {
        Self {
            policy: policy.normalized(),
            consecutive_failures: 0,
            consecutive_successes: 0,
            incident_started: None,
            incident_open: false,
        }
    }

    fn observe(
        &mut self,
        monitor: &str,
        outcome: &CheckOutcome,
        now: SystemTime,
    ) -> Option<AlertEvent> {
        match outcome.health {
            Health::Unhealthy => self.observe_failure(monitor, outcome, now),
            Health::Healthy => self.observe_success(monitor, outcome, now),
        }
    }

    fn observe_failure(
        &mut self,
        monitor: &str,
        outcome: &CheckOutcome,
        now: SystemTime,
    ) -> Option<AlertEvent> {
        self.consecutive_successes = 0;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.incident_started.get_or_insert(now);

        if self.incident_open || self.consecutive_failures < self.policy.failure_threshold {
            return None;
        }

        Some(AlertEvent {
            monitor: monitor.to_owned(),
            kind: AlertKind::Firing,
            summary: outcome.summary.clone(),
            consecutive_failures: self.consecutive_failures,
            incident_duration: None,
        })
    }

    fn observe_success(
        &mut self,
        monitor: &str,
        outcome: &CheckOutcome,
        now: SystemTime,
    ) -> Option<AlertEvent> {
        if !self.incident_open {
            self.reset();
            return None;
        }

        self.consecutive_successes = self.consecutive_successes.saturating_add(1);
        if self.consecutive_successes < self.policy.recovery_threshold {
            return None;
        }

        let incident_duration = self
            .incident_started
            .and_then(|started| now.duration_since(started).ok());
        Some(AlertEvent {
            monitor: monitor.to_owned(),
            kind: AlertKind::Recovered,
            summary: outcome.summary.clone(),
            consecutive_failures: self.consecutive_failures,
            incident_duration,
        })
    }

    fn mark_delivered(&mut self, kind: AlertKind) {
        match kind {
            AlertKind::Firing => self.incident_open = true,
            AlertKind::Recovered => self.reset(),
        }
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
        self.consecutive_successes = 0;
        self.incident_started = None;
        self.incident_open = false;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use argus_core::{AlertKind, CheckOutcome};

    use super::{AlertPolicy, IncidentTracker};

    fn unhealthy() -> CheckOutcome {
        CheckOutcome::unhealthy("timeout", Duration::from_secs(1))
    }

    fn healthy() -> CheckOutcome {
        CheckOutcome::healthy("ok", Duration::from_millis(10))
    }

    #[test]
    fn fires_once_after_threshold_and_recovers_once() {
        let policy = AlertPolicy {
            failure_threshold: 2,
            recovery_threshold: 2,
        };
        let mut tracker = IncidentTracker::new(policy);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        assert!(tracker.observe("api", &unhealthy(), now).is_none());
        let firing = tracker.observe("api", &unhealthy(), now + Duration::from_secs(1));
        assert_eq!(
            firing.as_ref().map(|event| event.kind),
            Some(AlertKind::Firing)
        );
        tracker.mark_delivered(AlertKind::Firing);
        assert!(
            tracker
                .observe("api", &unhealthy(), now + Duration::from_secs(2))
                .is_none()
        );
        assert!(
            tracker
                .observe("api", &healthy(), now + Duration::from_secs(3))
                .is_none()
        );
        let recovered = tracker.observe("api", &healthy(), now + Duration::from_secs(4));
        assert_eq!(
            recovered.as_ref().map(|event| event.kind),
            Some(AlertKind::Recovered)
        );
        tracker.mark_delivered(AlertKind::Recovered);
        assert!(
            tracker
                .observe("api", &healthy(), now + Duration::from_secs(5))
                .is_none()
        );
    }

    #[test]
    fn retries_an_alert_until_delivery_succeeds() {
        let mut tracker = IncidentTracker::new(AlertPolicy {
            failure_threshold: 1,
            recovery_threshold: 1,
        });
        let now = SystemTime::UNIX_EPOCH;

        assert!(tracker.observe("api", &unhealthy(), now).is_some());
        assert!(
            tracker
                .observe("api", &unhealthy(), now + Duration::from_secs(1))
                .is_some()
        );
    }
}
