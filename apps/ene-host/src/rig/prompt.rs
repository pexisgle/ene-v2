//! The host's model-request prompt slot.
//!
//! The host writes a prompt for a specific accepted input; the kernel reads it
//! when it builds that input's model request. Delivery is not held by the
//! semantic search, and a stale writer cannot replace a newer revision.
use rig_core::completion::SystemPrompt;
use rig_ecs::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PromptVersion {
    pub profile: u64,
    pub state: u64,
    pub recall: u64,
}

struct Entry {
    prompt: String,
    version: PromptVersion,
}

pub(crate) struct PromptState {
    entries: Mutex<HashMap<(String, String), Entry>>,
    recall: Mutex<HashMap<String, u64>>,
}

impl PromptState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            recall: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn next_recall(&self, companion: &str) -> u64 {
        let mut recall = self.recall.lock().expect("prompt recall lock");
        let value = recall.entry(companion.to_owned()).or_default();
        *value += 1;
        *value
    }

    /// Publish a prompt. `pending` means a model request may wait for a
    /// semantic update until `deadline`. An older version never replaces a
    /// newer one.
    pub(crate) fn offer(
        &self,
        companion: &str,
        request: &str,
        prompt: String,
        version: PromptVersion,
    ) {
        let mut entries = self.entries.lock().expect("prompt entries lock");
        let key = (companion.to_owned(), request.to_owned());
        if entries
            .get(&key)
            .is_some_and(|entry| entry.version > version)
        {
            return;
        }
        entries.insert(key, Entry { prompt, version });
    }

    /// Replace the prompt for an active input without releasing a pending
    /// semantic recall. A newer mind revision wins; an older one is ignored.
    pub(crate) fn update(
        &self,
        companion: &str,
        request: &str,
        prompt: String,
        version: PromptVersion,
    ) {
        let mut entries = self.entries.lock().expect("prompt entries lock");
        let key = (companion.to_owned(), request.to_owned());
        let Some(entry) = entries.get_mut(&key) else {
            return;
        };
        if entry.version > version {
            return;
        }
        entry.prompt = prompt;
        entry.version = version;
    }

    pub(crate) fn remove(&self, companion: &str, request: &str) {
        self.entries
            .lock()
            .expect("prompt entries lock")
            .remove(&(companion.to_owned(), request.to_owned()));
    }

    /// Read the prompt for a model request. No external search is awaited
    /// here; the host updates the slot before the next request.
    pub(crate) fn read(&self, companion: &str, request: &str) -> Option<String> {
        self.entries
            .lock()
            .expect("prompt entries lock")
            .get(&(companion.to_owned(), request.to_owned()))
            .map(|entry| entry.prompt.clone())
    }
}

/// The kernel resource carrying the host's prompt slot.
#[derive(Clone, Resource)]
pub(crate) struct PromptBridge(pub Arc<PromptState>);

/// Apply the host's prompt for the input this model request answers. This is
/// the last host-owned step before the request is fitted and sent; a full
/// prompt written by an older mind revision cannot overwrite a newer one.
pub(crate) fn apply_prompt(
    mut requests: Query<(&mut ModelRequest, &ForAgent)>,
    agents: Query<&AgentId, With<Agent>>,
    bridge: Option<Res<PromptBridge>>,
) {
    let Some(bridge) = bridge else {
        return;
    };
    for (mut request, agent) in &mut requests {
        let Ok(agent) = agents.get(agent.0) else {
            continue;
        };
        let Some(history) = request.history.as_ref() else {
            continue;
        };
        let Some(input) = history
            .entries()
            .rev()
            .find(|entry| entry.is_users_own())
            .and_then(|entry| entry.author)
            .and_then(|author| author.request.as_ref())
            .map(|request| request.0.clone())
        else {
            continue;
        };
        let Some(prompt) = bridge.0.read(&agent.0, &input) else {
            continue;
        };
        request.prompt = Some(SystemPrompt {
            role: prompt,
            sections: Vec::new(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(state: u64) -> PromptVersion {
        PromptVersion {
            profile: 1,
            state,
            recall: 1,
        }
    }

    #[test]
    fn an_older_version_never_overwrites_a_newer_prompt() {
        let state = PromptState::new();
        state.offer("c", "r", "new".into(), version(3));
        state.offer("c", "r", "old".into(), version(2));
        assert_eq!(state.read("c", "r").as_deref(), Some("new"));
        state.update("c", "r", "older".into(), version(1));
        assert_eq!(state.read("c", "r").as_deref(), Some("new"));
    }
}
