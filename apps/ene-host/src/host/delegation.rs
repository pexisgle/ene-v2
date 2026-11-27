//! Atomic delegation claims for one voice recognition event
//! (docs/design/client.md).
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
use hashlink::LruCache;

/// How many delivered transcriptions are remembered. Claims older than this
/// are forgotten, which at worst lets a stale, replayed utterance claim again;
/// the id is unique per capture, so that requires a duplicated stream.
const MAX_TRANSCRIPTIONS: usize = 256;

#[derive(Resource)]
pub(crate) struct DeliveredClaims(LruCache<String, Option<String>>);

impl Default for DeliveredClaims {
    fn default() -> Self {
        Self(LruCache::new(MAX_TRANSCRIPTIONS))
    }
}

impl DeliveredClaims {
    fn record(&mut self, transcription_id: &str) {
        if !self.0.contains_key(transcription_id) {
            self.0.insert(transcription_id.to_owned(), None);
        }
    }

    /// The first claim wins; the owner may claim again (a retried tool call is
    /// the same delegation). An id the voice pipeline never delivered is not
    /// arbitrated: it is a chat request id or a test, and returning `true`
    /// keeps manual chat working.
    fn try_claim(&mut self, transcription_id: &str, companion_id: &str) -> bool {
        let Some(owner) = self.0.peek_mut(transcription_id) else {
            return true;
        };
        owner
            .get_or_insert_with(|| companion_id.to_owned())
            .as_str()
            == companion_id
    }
}

/// Records a final transcript the voice pipeline delivered to the companions.
pub(crate) fn record(claims: &mut DeliveredClaims, transcription_id: &str) {
    if transcription_id.is_empty() {
        return;
    }
    claims.record(transcription_id);
}

/// Atomically claims `transcription_id` for `companion_id`. `true` means this
/// companion may start the work; `false` means another companion already did.
pub(crate) fn try_claim(
    claims: &mut DeliveredClaims,
    transcription_id: &str,
    companion_id: &str,
) -> bool {
    if transcription_id.is_empty() {
        return true;
    }
    claims.try_claim(transcription_id, companion_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replayed_transcriptions_do_not_renew_or_release_a_delegation() {
        let mut claims = DeliveredClaims::default();
        record(&mut claims, "first");
        assert!(try_claim(&mut claims, "first", "ene"));
        for id in 1..MAX_TRANSCRIPTIONS {
            record(&mut claims, &id.to_string());
        }
        record(&mut claims, "first");
        assert!(try_claim(&mut claims, "first", "ene"));
        assert!(!try_claim(&mut claims, "first", "other"));
        record(&mut claims, "next");
        assert!(try_claim(&mut claims, "first", "other"));
        assert!(try_claim(&mut claims, "1", "ene"));
        assert!(!try_claim(&mut claims, "1", "other"));
    }
}
