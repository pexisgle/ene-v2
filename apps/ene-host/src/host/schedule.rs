//! Persistent schedules. Firing is owned by the future autonomy tick; the
//! parent can already create, list and cancel rows.
use super::{
    agent::AgentKey,
    db::{Result, Store, new_id, now, validate_id},
    error::HostError,
    tools,
};
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Clone, Debug)]
pub(crate) struct Schedule {
    pub(crate) id: String,
    pub(crate) companion_id: String,
    pub(crate) cron_expr: Option<String>,
    pub(crate) run_at: Option<String>,
    pub(crate) title: String,
    pub(crate) instruction: String,
    pub(crate) enabled: bool,
    pub(crate) created_at: String,
    pub(crate) last_triggered_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    title: String,
    instruction: String,
    #[serde(default)]
    run_at: Option<String>,
    #[serde(default)]
    cron_expr: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    schedule_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

pub(crate) fn register_tools(app: &mut App) {
    app.add_open_tool::<CreateArgs, _>(
        "schedule_create",
        "Create a reminder or recurring schedule for this companion.",
        ToolOptions::default(),
        tools::run_tool(|world, agent, _, args| create_tool(world, agent, args)),
    )
    .add_open_tool::<Empty, _>(
        "schedule_list",
        "List this companion's schedules.",
        ToolOptions::default(),
        tools::run_tool(|world, agent, _, _: Empty| list_tool(world, agent)),
    )
    .add_open_tool::<CancelArgs, _>(
        "schedule_cancel",
        "Cancel one of this companion's schedules.",
        ToolOptions::default(),
        tools::run_tool(|world, agent, _, args| cancel_tool(world, agent, args)),
    );
}

fn create_tool(world: &mut World, agent: Entity, args: CreateArgs) -> Result<String> {
    let key = parent_key(world, agent)?;
    if args.title.trim().is_empty() || args.title.chars().count() > 200 {
        return Err(HostError::invalid("invalid schedule title"));
    }
    if args.instruction.trim().is_empty() || args.instruction.len() > 16_000 {
        return Err(HostError::invalid("invalid schedule instruction"));
    }
    if args.run_at.is_none() && args.cron_expr.is_none() {
        return Err(HostError::invalid("run_at or cron_expr is required"));
    }
    if args.run_at.as_ref().is_some_and(|value| value.len() > 100)
        || args
            .cron_expr
            .as_ref()
            .is_some_and(|value| value.len() > 100)
    {
        return Err(HostError::invalid("schedule time is too long"));
    }
    let schedule = Schedule {
        id: new_id(),
        companion_id: key.companion().into(),
        cron_expr: args.cron_expr,
        run_at: args.run_at,
        title: args.title,
        instruction: args.instruction,
        enabled: true,
        created_at: now(),
        last_triggered_at: None,
    };
    world.resource::<Store>().save_schedule(&schedule)?;
    Ok(schedule.id)
}

fn list_tool(world: &mut World, agent: Entity) -> Result<String> {
    let key = parent_key(world, agent)?;
    let items: Vec<_> = world
        .resource::<Store>()
        .schedules_for(key.companion())?
        .into_iter()
        .map(|schedule| {
            json!({
                "id": schedule.id,
                "title": schedule.title,
                "instruction": schedule.instruction,
                "run_at": schedule.run_at,
                "cron_expr": schedule.cron_expr,
                "enabled": schedule.enabled,
                "created_at": schedule.created_at,
                "last_triggered_at": schedule.last_triggered_at,
            })
        })
        .collect();
    Ok(serde_json::Value::Array(items).to_string())
}

fn cancel_tool(world: &mut World, agent: Entity, args: CancelArgs) -> Result<String> {
    let key = parent_key(world, agent)?;
    validate_id(&args.schedule_id)?;
    let schedule = world
        .resource::<Store>()
        .schedule(&args.schedule_id)?
        .ok_or_else(|| HostError::not_found("schedule not found"))?;
    if schedule.companion_id != key.companion() {
        return Err(HostError::not_found("schedule not found"));
    }
    world
        .resource::<Store>()
        .remove_schedule(&args.schedule_id)?;
    Ok("cancelled".into())
}

fn parent_key(world: &mut World, agent: Entity) -> Result<AgentKey> {
    let key = world
        .get::<AgentKey>(agent)
        .cloned()
        .ok_or_else(|| HostError::permission("unbound agent"))?;
    if key.is_work() {
        return Err(HostError::permission("not available to work agents"));
    }
    Ok(key)
}
