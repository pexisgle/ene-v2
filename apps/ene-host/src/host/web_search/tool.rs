use super::{
    DEFAULT_RESULTS, MAX_QUERY_CHARS, MAX_RESULTS, NAME, SearchBinding, SearchConfigError, execute,
};
use crate::host::{
    background, db::Store, model::Credentials, providers::Providers, resources::HostRuntime,
};
use bevy_ecs::{prelude::*, system::SystemParam};
use bevy_tasks::{AsyncComputeTaskPool, TaskPool};
use rig_core::tool::{ToolErrorKind, ToolExecutionError, ToolOutput, ToolResult, Touches};
use rig_ecs::prelude::*;
use serde::Deserialize;

pub(crate) struct SearchPlugin;

impl Plugin for SearchPlugin {
    fn build(&self, app: &mut App) {
        app.add_open_tool::<SearchArgs, _>(
            NAME,
            "Search the public Web using the configured search connection and return titles, \
             source URLs and snippets. Cite the source URLs when reporting findings.",
            // A search reads public results but sends the user's query. Restoring
            // an interrupted turn must not repeat that external transfer.
            ToolOptions {
                footprint: Some(Touches::Independent),
            },
            |called: On<ToolCalled<SearchArgs>>,
             bindings: Query<&SearchBinding>,
             decisions: Query<&mut Decision>,
             services: SearchServices<'_>,
             commands: Commands| {
                search(called.event(), bindings, decisions, &services, commands);
            },
        );
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    #[schemars(length(min = 1, max = MAX_QUERY_CHARS))]
    query: String,
    #[schemars(range(min = 1, max = MAX_RESULTS))]
    limit: Option<usize>,
}

#[cfg(test)]
pub(super) fn argument_schema() -> serde_json::Value {
    rig_core::tool::args_schema::<SearchArgs>()
}

#[derive(SystemParam)]
struct SearchServices<'w> {
    store: Res<'w, Store>,
    credentials: Option<Res<'w, Credentials>>,
    providers: Option<Res<'w, Providers>>,
    runtime: Option<Res<'w, HostRuntime>>,
    wake: Res<'w, Wake>,
}

fn search(
    called: &ToolCalled<SearchArgs>,
    bindings: Query<&SearchBinding>,
    mut decisions: Query<&mut Decision>,
    services: &SearchServices<'_>,
    mut commands: Commands,
) {
    match start(bindings.get(called.call).ok(), &called.args, services) {
        Ok(running) => {
            commands.entity(called.call).insert(running);
        }
        Err(error) => {
            if let Ok(mut decision) = decisions.get_mut(called.call) {
                decision.decide(
                    Decider::plugin("ene-host"),
                    Outcome::refused(error.to_string()),
                );
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum SearchStartError {
    #[error("the permission gate did not bind a search connection")]
    NotBound,
    #[error("query must be 1–{MAX_QUERY_CHARS} characters")]
    QueryLength,
    #[error("limit must be 1–{MAX_RESULTS}")]
    Limit,
    #[error(transparent)]
    Connection(#[from] SearchConfigError),
    #[error("the host HTTP runtime is unavailable")]
    Runtime,
}

fn start(
    binding: Option<&SearchBinding>,
    args: &SearchArgs,
    services: &SearchServices<'_>,
) -> Result<Running, SearchStartError> {
    let binding = binding.ok_or(SearchStartError::NotBound)?;
    let query = args.query.trim();
    if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS {
        return Err(SearchStartError::QueryLength);
    }
    let limit = args.limit.unwrap_or(DEFAULT_RESULTS);
    if !(1..=MAX_RESULTS).contains(&limit) {
        return Err(SearchStartError::Limit);
    }
    let fallback = Providers::default();
    let resolved = binding.current(
        &services.store,
        services.credentials.as_deref(),
        services.providers.as_deref().unwrap_or(&fallback),
    )?;
    let runtime = services
        .runtime
        .as_ref()
        .map(|runtime| runtime.0.clone())
        .or_else(|| tokio::runtime::Handle::try_current().ok())
        .ok_or(SearchStartError::Runtime)?;
    let query = query.to_owned();
    let limit = resolved
        .max_results
        .map_or(limit, |maximum| maximum.min(limit));
    let task = background::spawn(&runtime, None, async move {
        execute(&resolved, &query, limit).await
    });
    Ok(Running::answering(
        AsyncComputeTaskPool::get_or_init(TaskPool::default),
        &services.wake,
        async move {
            match task.await {
                Ok(Ok(result)) => Outcome::from(ToolResult::success(ToolOutput::json(result))),
                Ok(Err(error)) => Outcome::from(ToolResult::failed(error)),
                Err(_) => Outcome::from(ToolResult::failed(ToolExecutionError::new(
                    ToolErrorKind::Other,
                    "the web search task stopped",
                ))),
            }
        },
    ))
}
