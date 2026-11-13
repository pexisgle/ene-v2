//! Context compaction.
//!
//! The session log is append-only and never rewritten. The request view is
//! trimmed in `mind::recall::fit`, and the summary of dropped history is kept
//! as an `ene.summary` aside. The background summarizer is not wired in this
//! alpha: the window keeps the newest complete turns and refuses a request
//! that cannot fit at all.
use rig_ecs::prelude::*;

pub(crate) struct CompactionPlugin;

impl Plugin for CompactionPlugin {
    fn build(&self, _app: &mut App) {}
}
