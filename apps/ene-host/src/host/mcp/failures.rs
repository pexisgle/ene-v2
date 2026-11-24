//! Backoff, jitter and configuration-hold bookkeeping for failing MCP
//! connections.
use ene_work::mcp::{Desired, Launch, backoff_delay, is_config_error_message};
use rig_ecs::prelude::*;
use std::{collections::HashMap, time::Instant};
use tokio_util::task::AbortOnDropHandle;

pub(super) enum FailureKind {
    Transient {
        attempts: u32,
        next_retry: Instant,
        _wake_timer: Option<AbortOnDropHandle<()>>,
    },
    ConfigHold,
}

pub(super) struct FailureState {
    pub(super) revision: u64,
    pub(super) credential_generation: Option<u64>,
    pub(super) reason: String,
    pub(super) kind: FailureKind,
}

/// Backoff and configuration-hold tracking per failing MCP connection.
#[derive(Default)]
pub(super) struct McpFailures(pub(super) HashMap<String, FailureState>);

pub(super) fn prune_failures(failures: &mut McpFailures, wanted: &[(String, Desired)]) {
    // Clear failure holds/backoffs when connection config or credentials changed, or connection left wanted.
    failures.0.retain(|id, failure| {
        wanted.iter().any(|(wanted, want)| {
            wanted == id
                && want.revision == failure.revision
                && want.credential_generation == failure.credential_generation
        })
    });
}

pub(super) struct FailureReport {
    pub(super) id: String,
    pub(super) launch: Launch,
    pub(super) reason: String,
    pub(super) config_error: bool,
}

/// Spreads a retry delay over the second half of its window.
///
/// Several MCP servers retry inside one host process on timers that start at
/// the same moment, so a host restart makes them all come back together.
/// Jitter is applied here, where the delay is scheduled, rather than inside
/// `backoff_delay`, which stays a pure function a test can assert exactly.
fn with_jitter(delay: std::time::Duration) -> std::time::Duration {
    let window = delay.as_millis() as u64 / 2;
    if window == 0 {
        return delay;
    }
    let mut bytes = [0u8; 8];
    // A failed entropy read falls back to no jitter: a slightly synchronised
    // retry is not worth failing a reconnect over.
    if getrandom::fill(&mut bytes).is_err() {
        return delay;
    }
    let offset = u64::from_le_bytes(bytes) % window;
    delay - std::time::Duration::from_millis(offset)
}

pub(super) fn record_failure(
    failures: &mut McpFailures,
    report: FailureReport,
    wake: Option<Wake>,
    runtime: &tokio::runtime::Handle,
) {
    let FailureReport {
        id,
        launch,
        reason,
        config_error,
    } = report;
    let kind = if config_error || is_config_error_message(&reason) {
        FailureKind::ConfigHold
    } else {
        let prev_attempts = match failures.0.get(&id) {
            Some(prev)
                if prev.revision == launch.revision
                    && prev.credential_generation == launch.credential_generation =>
            {
                match prev.kind {
                    FailureKind::Transient { attempts, .. } => attempts,
                    FailureKind::ConfigHold => 0,
                }
            }
            _ => 0,
        };
        let attempts = prev_attempts.saturating_add(1);
        let delay = with_jitter(backoff_delay(attempts));
        let next_retry = Instant::now() + delay;
        let wake_timer = wake.map(|wake| {
            AbortOnDropHandle::new(runtime.spawn(async move {
                tokio::time::sleep(delay).await;
                wake.wake();
            }))
        });
        FailureKind::Transient {
            attempts,
            next_retry,
            _wake_timer: wake_timer,
        }
    };
    failures.0.insert(
        id,
        FailureState {
            revision: launch.revision,
            credential_generation: launch.credential_generation,
            reason,
            kind,
        },
    );
}

#[cfg(test)]
mod jitter_tests {
    use super::with_jitter;
    use std::time::Duration;

    /// Jitter must never extend a retry, and must stay inside the second half
    /// of the window, so a delay of 32s lands somewhere in 16..=32s.
    #[test]
    fn jitter_stays_inside_the_lower_half_of_the_window() {
        let delay = Duration::from_secs(32);
        let floor = delay - Duration::from_secs(16);
        for _ in 0..200 {
            let jittered = with_jitter(delay);
            assert!(
                jittered <= delay,
                "jitter must not extend the delay: {jittered:?} > {delay:?}"
            );
            assert!(
                jittered >= floor,
                "jitter must stay in the lower half: {jittered:?} < {floor:?}"
            );
        }
    }

    /// A delay too short to have a half-millisecond window is returned as-is
    /// rather than divided down to zero.
    #[test]
    fn a_sub_millisecond_delay_is_unchanged() {
        let delay = Duration::from_micros(500);
        assert_eq!(with_jitter(delay), delay);
        assert_eq!(with_jitter(Duration::ZERO), Duration::ZERO);
    }
}
