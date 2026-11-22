//! Procedural skills for companions and work agents (`F06`).
//!
//! Skills capture reusable step-by-step procedures scoped to a companion in
//! SQLite (`skills` table) and are exposed through `skill_list`, `skill_read`,
//! and `skill_save`.
use crate::host::{
    agent::{CompanionAgent, WorkAgent},
    db::{Result, Store},
    error::HostError,
    tools::run_tool,
};
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillListArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillReadArgs {
    /// Name of the procedural skill to read.
    pub(crate) name: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillSaveArgs {
    /// Stable slug (`[a-z0-9_-]`, 1-64 characters).
    pub(crate) name: String,
    /// Short one-line summary of when to use this skill.
    pub(crate) description: String,
    /// Detailed procedural instructions.
    pub(crate) instructions: String,
}

pub(crate) fn register(app: &mut App) {
    app.add_open_tool::<SkillListArgs, _>(
        "skill_list",
        "List saved procedural skills for this companion.",
        ToolOptions::default(),
        run_tool(|world, agent, _, _| list_tool(world, agent)),
    )
    .add_open_tool::<SkillReadArgs, _>(
        "skill_read",
        "Read the full instructions of one saved procedural skill.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| read_tool(world, agent, args)),
    )
    .add_open_tool::<SkillSaveArgs, _>(
        "skill_save",
        "Save or update a reusable procedural skill for this companion.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| save_tool(world, agent, args)),
    );
}

fn owner_companion(world: &World, agent: Entity) -> Result<String> {
    if let Some(c) = world.get::<CompanionAgent>(agent) {
        return Ok(c.companion_id.clone());
    }
    if let Some(w) = world.get::<WorkAgent>(agent) {
        return Ok(w.companion_id.clone());
    }
    Err(HostError::Internal(
        "skill tools belong to a companion or work agent".into(),
    ))
}

fn list_tool(world: &mut World, agent: Entity) -> Result<String> {
    let companion_id = owner_companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let items: Vec<serde_json::Value> = store
        .skill_repo()
        .list_skills(&companion_id)?
        .into_iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "updated_at": s.updated_at,
            })
        })
        .collect();
    Ok(json!({ "skills": items }).to_string())
}

fn read_tool(world: &mut World, agent: Entity, args: SkillReadArgs) -> Result<String> {
    let companion_id = owner_companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let skill = store
        .skill_repo()
        .read_skill(&companion_id, &args.name)?
        .ok_or_else(|| HostError::NotFound(format!("skill `{}` not found", args.name)))?;
    Ok(json!(skill).to_string())
}

fn save_tool(world: &mut World, agent: Entity, args: SkillSaveArgs) -> Result<String> {
    let companion_id = owner_companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let saved = store.skill_repo().save_skill(
        &companion_id,
        &args.name,
        &args.description,
        &args.instructions,
    )?;
    Ok(json!({ "saved": true, "skill": saved }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::db::memories::MemoryRecord;
    use crate::host::tests::Harness;
    use rig_core::completion::Message;
    use rig_core::message::{ToolResultContent, UserContent};
    use rig_core::test_utils::MockStreamEvent;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn t15_skills_save_list_read_and_work_agent_mind_recall() {
        let mut harness = Harness::new([
            vec![
                MockStreamEvent::tool_call(
                    "c1",
                    "skill_save",
                    json!({
                        "name": "release-check",
                        "description": "リリース前確認手順",
                        "instructions": "1. just fmt 2. just verify 3. just test"
                    }),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::tool_call("c2", "skill_read", json!({"name": "release-check"})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::tool_call("c3", "mind_recall", json!({"query": "リリース手順"})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("確認完了"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("了解"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let store = harness.store();
        store
            .memory_repo()
            .insert_memory(MemoryRecord {
                id: "m-release".into(),
                scope: harness.companion.clone(),
                content: "リリース手順 は必ず verify を通す".into(),
                importance: 9,
                occurred_at: None,
                source_context: json!({}),
            })
            .unwrap();

        crate::host::work::start(
            harness.app.world_mut(),
            ene_rpc::StartWorkRequest {
                companion_id: harness.companion.clone(),
                instruction: "手順を保存して確認して".into(),
                ..Default::default()
            },
        )
        .unwrap();

        harness.run_until(|world| {
            world
                .query_filtered::<&Conversation, With<WorkAgent>>()
                .iter(world)
                .any(|conv| {
                    let saw_skill_read = conv.entries().any(|e| match &e.message {
                        Message::User { content } => content.iter().any(|item| {
                            matches!(item, UserContent::ToolResult(r)
                                if r.name.as_str() == "skill_read"
                                    && !r.is_error
                                    && r.content.iter().any(|b| matches!(b, ToolResultContent::Text(t) if t.text.contains("just verify"))))
                        }),
                        _ => false,
                    });
                    let saw_mind_recall = conv.entries().any(|e| match &e.message {
                        Message::User { content } => content.iter().any(|item| {
                            matches!(item, UserContent::ToolResult(r)
                                if r.name.as_str() == "mind_recall"
                                    && !r.is_error
                                    && r.content.iter().any(|b| matches!(b, ToolResultContent::Text(t) if t.text.contains("m-release"))))
                        }),
                        _ => false,
                    });
                    saw_skill_read && saw_mind_recall
                })
        });

        let skills = store.skill_repo().list_skills(&harness.companion).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "release-check");
    }
}
