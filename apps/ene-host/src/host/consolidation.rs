//! The incremental memory-consolidation cursor.
use super::db::now;

#[derive(Clone, Debug)]
pub(crate) struct Consolidation {
    pub(crate) last_sequence: u64,
    pub(crate) last_offset: u64,
    pub(crate) method_revision: String,
    pub(crate) state: String,
    pub(crate) reason: Option<String>,
    pub(crate) jobs: u64,
    pub(crate) model_calls: u64,
    pub(crate) input_chars: u64,
    pub(crate) skipped: u64,
    pub(crate) attempts: u64,
    pub(crate) updated_at: String,
}

impl Default for Consolidation {
    fn default() -> Self {
        Self {
            last_sequence: 0,
            last_offset: 0,
            method_revision: String::new(),
            state: "unconfigured".into(),
            reason: None,
            jobs: 0,
            model_calls: 0,
            input_chars: 0,
            skipped: 0,
            attempts: 0,
            updated_at: now(),
        }
    }
}
