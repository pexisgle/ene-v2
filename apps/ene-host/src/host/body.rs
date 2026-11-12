//! Body and eye tools exposed to the parent agent.
use super::{agent::AgentKey, client, db::Result, error::HostError, tools};
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EyeWatchArgs {
    target_pattern: String,
    match_kind: String,
    notify_on: String,
    enabled: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BodyMotionArgs {
    motion: String,
    #[serde(default)]
    expression: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

pub(crate) fn register_tools(app: &mut App) {
    app.add_open_tool::<EyeWatchArgs, _>(
        "set_eye_watch",
        "Set or clear a client-side perception watch rule.",
        ToolOptions::default(),
        tools::run_tool(|world, agent, _, args| eye_watch(world, agent, args)),
    )
    .add_open_tool::<BodyMotionArgs, _>(
        "body_motion",
        "Ask the avatar client to play a motion or expression.",
        ToolOptions::default(),
        tools::run_tool(|world, agent, _, args| body_motion(world, agent, args)),
    )
    .add_open_tool::<Empty, _>(
        "noop",
        "Do nothing. Used for deliberate silence in autonomous ticks.",
        ToolOptions::default(),
        tools::run_tool(|_, _, _, _: Empty| Ok("noop".to_owned())),
    );
}

fn eye_watch(world: &mut World, agent: Entity, args: EyeWatchArgs) -> Result<String> {
    let key = parent_key(world, agent)?;
    if args.target_pattern.is_empty() || args.target_pattern.len() > 1024 {
        return Err(HostError::invalid("invalid eye watch pattern"));
    }
    if !matches!(args.match_kind.as_str(), "title" | "app_id") {
        return Err(HostError::invalid("invalid match_kind"));
    }
    if !matches!(args.notify_on.as_str(), "focus" | "blur" | "title_change") {
        return Err(HostError::invalid("invalid notify_on"));
    }
    client::send_operation(
        world,
        key.companion(),
        "eye.watch",
        json!({
            "target_pattern": args.target_pattern,
            "match_kind": args.match_kind,
            "notify_on": args.notify_on,
            "enabled": args.enabled,
        })
        .to_string(),
    )
}

fn body_motion(world: &mut World, agent: Entity, args: BodyMotionArgs) -> Result<String> {
    let key = parent_key(world, agent)?;
    if args.motion.is_empty() || args.motion.len() > 200 {
        return Err(HostError::invalid("invalid motion"));
    }
    client::send_operation(
        world,
        key.companion(),
        "body.motion",
        json!({
            "motion": args.motion,
            "expression": args.expression,
        })
        .to_string(),
    )
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
