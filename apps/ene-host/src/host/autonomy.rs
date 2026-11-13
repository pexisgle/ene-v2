//! Small parent tools that the design declares before their engines exist:
//! `noop` and the schedule registry. Firing schedules and staged promotion
//! remain unimplemented in this pass; only the storage and its validation
//! (exactly one of `run_at`/`cron_expr`, RFC 3339 `run_at`, five-field cron
//! ranges) are wired.
use crate::host::{
    agent::CompanionAgent,
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
pub(crate) struct NoopArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateArgs {
    pub(crate) title: String,
    /// What the head should do when the schedule fires.
    pub(crate) instruction: String,
    /// A one-shot time in RFC 3339.
    #[serde(default)]
    pub(crate) run_at: Option<String>,
    /// A repeating 5-field cron expression.
    #[serde(default)]
    pub(crate) cron_expr: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ListArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CancelArgs {
    pub(crate) schedule_id: String,
}

pub(crate) fn register(app: &mut App) {
    app.add_open_tool::<NoopArgs, _>(
        "noop",
        "Do nothing this turn: stay silent and keep watching.",
        ToolOptions::default(),
        run_tool(|_: &mut World, _: Entity, _: &str, _: NoopArgs| {
            Ok(json!({"ok": true}).to_string())
        }),
    )
    .add_open_tool::<CreateArgs, _>(
        "schedule_create",
        "Register a reminder or repeating schedule.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| create(world, agent, args)),
    )
    .add_open_tool::<ListArgs, _>(
        "schedule_list",
        "List your active schedules.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| list(world, agent, args)),
    )
    .add_open_tool::<CancelArgs, _>(
        "schedule_cancel",
        "Remove one of your schedules.",
        ToolOptions::default(),
        run_tool(|world, agent, _, args| cancel(world, agent, args)),
    );
}

fn companion(world: &World, agent: Entity) -> Result<String> {
    world
        .get::<CompanionAgent>(agent)
        .map(|agent| agent.companion_id.clone())
        .ok_or_else(|| HostError::internal("this tool belongs to a companion"))
}

fn create(world: &mut World, agent: Entity, args: CreateArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let schedule = store.insert_schedule(
        &companion,
        &args.title,
        &args.instruction,
        args.run_at.as_deref(),
        args.cron_expr.as_deref(),
    )?;
    Ok(json!({"schedule_id": schedule.id}).to_string())
}

fn list(world: &mut World, agent: Entity, _args: ListArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let schedules: Vec<serde_json::Value> = store
        .schedules(&companion)?
        .into_iter()
        .map(|schedule| {
            json!({
                "schedule_id": schedule.id,
                "title": schedule.title,
                "run_at": schedule.run_at,
                "cron_expr": schedule.cron_expr,
            })
        })
        .collect();
    Ok(json!({"schedules": schedules}).to_string())
}

fn cancel(world: &mut World, agent: Entity, args: CancelArgs) -> Result<String> {
    let companion = companion(world, agent)?;
    let store = world.resource::<Store>().clone();
    let removed = store.cancel_schedule(&companion, &args.schedule_id)?;
    Ok(json!({"schedule_id": args.schedule_id, "removed": removed}).to_string())
}

/// Registers the small parent tools.
pub(crate) struct AutonomyPlugin;

impl Plugin for AutonomyPlugin {
    fn build(&self, app: &mut App) {
        register(app);
    }
}
