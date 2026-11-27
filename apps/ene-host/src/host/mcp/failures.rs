//! Backoff, jitter and configuration-hold bookkeeping for failing MCP
//! connections.
use super::settings::{ConnectionStamp, DesiredServers};
use ene_work::mcp::{ConnectFailure, backoff_delay};
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
    pub(super) stamp: ConnectionStamp,
    pub(super) failure: ConnectFailure,
    pub(super) kind: FailureKind,
}

/// Backoff and configuration-hold tracking per failing MCP connection.
#[derive(Default)]
pub(super) struct McpFailures(pub(super) HashMap<String, FailureState>);

pub(super) fn prune_failures(failures: &mut McpFailures, wanted: &DesiredServers) {
    failures.0.retain(|id, failure| {
        wanted
            .get(id)
            .is_some_and(|want| want.stamp == failure.stamp)
    });
}

pub(super) fn record_failure(
    failures: &mut McpFailures,
    id: String,
    stamp: ConnectionStamp,
    failure: ConnectFailure,
    wake: Option<Wake>,
    runtime: &tokio::runtime::Handle,
) {
    let kind = if matches!(failure, ConnectFailure::Configuration(_)) {
        FailureKind::ConfigHold
    } else {
        let prev_attempts = match failures.0.get(&id) {
            Some(prev) if prev.stamp == stamp => match prev.kind {
                FailureKind::Transient { attempts, .. } => attempts,
                FailureKind::ConfigHold => 0,
            },
            _ => 0,
        };
        let attempts = prev_attempts.saturating_add(1);
        let delay = backoff_delay(attempts);
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
            stamp,
            failure,
            kind,
        },
    );
}
