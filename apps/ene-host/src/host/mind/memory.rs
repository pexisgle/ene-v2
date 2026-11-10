//! Episodic MemoryRecord, audience sharing/forgetting control and automatic recall injection.
use super::{CompanionRecord, deliver_state, mind_view, notify_mind};
use crate::host::{
    Host,
    db::{Result, invalid, next_revision, not_found, now},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::conversation::Author;
use rig_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const PRIVATE: &str = "private";
pub(crate) const SHARED: &str = "shared";
pub(crate) const NORMAL: &str = "normal";
pub(crate) const FORGOTTEN: &str = "forgotten";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MemoryRecord {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) kind: String,
    pub(crate) audience: String,
    pub(crate) content: String,
    pub(crate) description: String,
    pub(crate) appraisal: String,
    pub(crate) acquisition: String,
    #[serde(default)]
    pub(crate) sources: Vec<Value>,
    pub(crate) recorded_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) occurred_at: Option<String>,
    #[serde(default)]
    pub(crate) supersedes: Vec<String>,
    #[serde(default = "default_normal")]
    pub(crate) recall_preference: String,
    pub(crate) created_by: String,
    pub(crate) revision: u64,
    pub(crate) updated_at: String,
}

fn default_normal() -> String {
    NORMAL.into()
}

impl MemoryRecord {
    pub(crate) fn visible_to(&self, viewer: &str) -> bool {
        self.owner == viewer
            || self.audience == SHARED
            || self.audience == format!("companion:{viewer}")
    }

    pub(crate) fn view_for(&self, viewer: &str) -> Value {
        let owner = self.owner == viewer;
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(obj) = value.as_object_mut() {
            obj.remove("updated_at");
            if !owner {
                obj.insert("owner".into(), Value::Null);
                obj.insert("sources".into(), Value::Array(Vec::new()));
            }
        }
        value
    }
}

pub(crate) fn control_memory(
    world: &mut World,
    request: w::MemoryControlRequest,
) -> Result<w::MemoryControlResult> {
    let db = &world.resource::<Host>().db;
    let memory = db.transaction(|tx| {
        let mut memory = tx
            .get_memory(&request.memory_id)?
            .filter(|m| m.owner == request.companion_id)
            .ok_or_else(|| not_found("memory not found"))?;
        let (audience, preference) = match request.action.as_str() {
            "forget" => (memory.audience.clone(), FORGOTTEN.to_owned()),
            "remember" => (memory.audience.clone(), NORMAL.to_owned()),
            "share" => (SHARED.to_owned(), memory.recall_preference.clone()),
            "unshare" => (PRIVATE.to_owned(), memory.recall_preference.clone()),
            _ => return Err(invalid("unknown memory action")),
        };
        if audience != memory.audience || preference != memory.recall_preference {
            memory.audience = audience;
            memory.recall_preference = preference;
            memory.revision = next_revision(memory.revision)?;
            memory.updated_at = now();
            tx.put_memory(&memory)?;
            let key = format!("companion:{}", request.companion_id);
            let mut record: CompanionRecord = tx
                .get(&key)?
                .ok_or_else(|| not_found("companion not found"))?;
            record.memory_revision = next_revision(record.memory_revision)?;
            record.companion.revision = next_revision(record.companion.revision)?;
            record.companion.updated_at = now();
            tx.put(&key, &record)?;
        }
        Ok(memory)
    })?;
    let view = mind_view(&world.resource::<Host>().db, &request.companion_id)?;
    notify_mind(world, &request.companion_id, view);
    Ok(w::MemoryControlResult {
        memory_id: memory.id,
        audience: memory.audience,
        recall_preference: memory.recall_preference,
        revision: memory.revision,
        ..Default::default()
    })
}

pub(crate) fn inject_recall(
    world: &mut World,
    agent: Entity,
    companion_id: &str,
    text: &str,
) -> Result<()> {
    deliver_state(world, agent, companion_id)?;
    let memories = world
        .resource::<Host>()
        .db
        .search_memories(companion_id, text, false, 3)?;
    if !memories.is_empty() {
        let list = memories
            .iter()
            .map(|m| format!("- {}", m.content))
            .collect::<Vec<_>>()
            .join("\n");
        world.trigger(
            Deliver::new(
                agent,
                format!("関連する記憶:\n{list}"),
                DeliveryMode::Inform,
            )
            .with_author(Author::plugin("mind")),
        );
    }
    Ok(())
}
