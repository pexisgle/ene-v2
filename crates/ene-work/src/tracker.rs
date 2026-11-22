//! Work run stall tracking, run construction, and `SendWork` idempotency receipts.

use ene_domain::{Run, RunState};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// Inactivity threshold after which a running work session notifies its companion once.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(90);

/// Constructs a fresh domain [`Run`] ready to insert into persistent storage.
pub fn new_run(run_id: String, work_id: &str, instruction: &str, state: RunState) -> Run {
    Run {
        run_id,
        work_id: work_id.to_owned(),
        instruction: instruction.to_owned(),
        state,
        pending_request_id: None,
        failure: None,
        started_at: String::new(),
        updated_at: String::new(),
    }
}

#[derive(Debug)]
struct Live {
    last: Instant,
    warned: bool,
}

impl Live {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            warned: false,
        }
    }
}

/// Tracks active runs and fires once when a run is silent past `STALL_TIMEOUT`.
#[derive(Debug, Default)]
pub struct RunTracker {
    runs: HashMap<String, Live>,
}

impl RunTracker {
    pub fn begin(&mut self, run_id: &str) {
        self.runs.insert(run_id.to_owned(), Live::new());
    }

    pub fn touch(&mut self, run_id: &str) {
        let Some(live) = self.runs.get_mut(run_id) else {
            return;
        };
        live.last = Instant::now();
        live.warned = false;
    }

    pub fn forget(&mut self, run_id: &str) {
        self.runs.remove(run_id);
    }

    pub fn stalled(&mut self, now: Instant, threshold: Duration) -> Vec<String> {
        self.runs
            .iter_mut()
            .filter_map(|(run_id, live)| {
                if live.warned || now.saturating_duration_since(live.last) < threshold {
                    return None;
                }
                live.warned = true;
                Some(run_id.clone())
            })
            .collect()
    }
}

/// `SendWork` idempotency receipts held in memory while a work agent is absent.
#[derive(Debug, Default)]
pub struct WorkReceipts {
    entries: HashMap<String, (Vec<u8>, String)>,
}

impl WorkReceipts {
    pub fn get(&self, work_id: &str, request_key: &str) -> Option<(Vec<u8>, String)> {
        self.entries
            .get(&format!("{work_id}:{request_key}"))
            .cloned()
    }

    pub fn put(&mut self, work_id: &str, request_key: &str, hash: &[u8], request_id: &str) {
        self.entries.insert(
            format!("{work_id}:{request_key}"),
            (hash.to_vec(), request_id.to_owned()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_warns_once_per_stall_until_touched_and_receipts_round_trip() {
        let mut tracker = RunTracker::default();
        tracker.begin("r1");
        let later = Instant::now() + Duration::from_secs(100);
        assert_eq!(tracker.stalled(later, STALL_TIMEOUT), vec!["r1"]);
        assert!(tracker.stalled(later, STALL_TIMEOUT).is_empty());

        tracker.touch("r1");
        assert!(tracker.stalled(Instant::now(), STALL_TIMEOUT).is_empty());
        tracker.forget("r1");
        assert!(tracker.stalled(later, STALL_TIMEOUT).is_empty());

        let mut receipts = WorkReceipts::default();
        receipts.put("w1", "k1", &[1, 2, 3], "req-1");
        assert_eq!(
            receipts.get("w1", "k1"),
            Some((vec![1, 2, 3], "req-1".into()))
        );
        assert_eq!(receipts.get("w1", "other"), None);
    }
}
