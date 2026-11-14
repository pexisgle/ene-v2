//! Atomic delegation claims for one voice recognition event
//! (docs/design/voice-avatar.md §5.2).
//!
//! Every companion receives the same transcription, so two companions may
//! decide to start the same irreversible task. The host binds the task to the
//! transcription id instead of trusting an LLM-generated key: the first
//! companion to call `start_work` in a turn that answers a delivered
//! transcription claims it, and every other companion is told to support the
//! owner instead of duplicating the work. A manual chat message carries a
//! request id too, so only a transcription the voice pipeline recorded here is
//! ever claimed.
use bevy_ecs::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};

/// How many delivered transcriptions are remembered. Claims older than this
/// are forgotten, which at worst lets a stale, replayed utterance claim again;
/// the id is unique per capture, so that requires a duplicated stream.
const MAX_TRANSCRIPTIONS: usize = 256;

#[derive(Default)]
struct Claims {
    /// Delivery order, oldest first, so the set stays bounded.
    delivered: VecDeque<String>,
    /// Transcription ids the voice pipeline delivered.
    known: HashSet<String>,
    /// Transcription id -> the companion that claimed it.
    owners: HashMap<String, String>,
}

impl Claims {
    fn record(&mut self, transcription_id: &str) {
        if !self.known.insert(transcription_id.to_owned()) {
            return;
        }
        self.delivered.push_back(transcription_id.to_owned());
        while self.delivered.len() > MAX_TRANSCRIPTIONS {
            let Some(oldest) = self.delivered.pop_front() else {
                break;
            };
            self.known.remove(&oldest);
            self.owners.remove(&oldest);
        }
    }

    /// The first claim wins; the owner may claim again (a retried tool call is
    /// the same delegation). An id the voice pipeline never delivered is not
    /// arbitrated: it is a chat request id or a test, and returning `true`
    /// keeps manual chat working.
    fn try_claim(&mut self, transcription_id: &str, companion_id: &str) -> bool {
        if !self.known.contains(transcription_id) {
            return true;
        }
        match self.owners.get(transcription_id) {
            Some(owner) => owner == companion_id,
            None => {
                self.owners
                    .insert(transcription_id.to_owned(), companion_id.to_owned());
                true
            }
        }
    }
}

/// The host's per-transcription delegation registry. A host without the voice
/// plugin has no resource, so nothing is ever claimed there.
#[derive(Resource, Default)]
pub(crate) struct Delegation(Claims);

/// Records a final transcript the voice pipeline delivered to the companions.
pub(crate) fn record(world: &mut World, transcription_id: &str) {
    if transcription_id.is_empty() {
        return;
    }
    world
        .get_resource_or_insert_with(Delegation::default)
        .0
        .record(transcription_id);
}

/// Atomically claims `transcription_id` for `companion_id`. `true` means this
/// companion may start the work; `false` means another companion already did.
pub(crate) fn try_claim(world: &mut World, transcription_id: &str, companion_id: &str) -> bool {
    if transcription_id.is_empty() {
        return true;
    }
    let Some(mut delegation) = world.get_resource_mut::<Delegation>() else {
        return true;
    };
    delegation.0.try_claim(transcription_id, companion_id)
}
