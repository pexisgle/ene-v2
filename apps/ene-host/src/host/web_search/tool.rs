use super::{DEFAULT_RESULTS, MAX_QUERY_CHARS, MAX_RESULTS, NAME, PreparedSearch, execute};
use crate::host::{
    background, db::Store, model::Credentials, permission::Approved, providers::Providers,
    resources::HostRuntime,
};
use bevy_ecs::{prelude::*, system::SystemParam};
use bevy_tasks::{AsyncComputeTaskPool, TaskPool};
use ene_policy::{Verdict, decide_action};
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
             bindings: Query<BoundSearch>,
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
pub(super) struct SearchArgs {
    #[schemars(length(min = 1, max = MAX_QUERY_CHARS))]
    query: String,
    #[schemars(range(min = 1, max = MAX_RESULTS))]
    limit: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SearchInputError {
    #[error("search arguments must contain a query and an optional limit")]
    Shape,
    #[error("query must be 1–{MAX_QUERY_CHARS} characters")]
    QueryLength,
    #[error("limit must be 1–{MAX_RESULTS}")]
    Limit,
    #[error("a search query cannot include the connection credential")]
    CredentialInQuery,
}

impl TryFrom<&serde_json::Value> for SearchArgs {
    type Error = SearchInputError;

    fn try_from(value: &serde_json::Value) -> Result<Self, Self::Error> {
        Self::deserialize(value).map_err(|_| SearchInputError::Shape)
    }
}

impl SearchArgs {
    pub(super) fn effective(
        &self,
        maximum: Option<usize>,
    ) -> Result<(&str, usize), SearchInputError> {
        let query = self.query.trim();
        if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS {
            return Err(SearchInputError::QueryLength);
        }
        let limit = self.limit.unwrap_or(DEFAULT_RESULTS);
        if !(1..=MAX_RESULTS).contains(&limit) {
            return Err(SearchInputError::Limit);
        }
        Ok((query, maximum.map_or(limit, |cap| cap.min(limit))))
    }
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

type BoundSearch<'a> = (&'a PreparedSearch, Option<&'a Approved>);

fn search(
    called: &ToolCalled<SearchArgs>,
    bindings: Query<BoundSearch>,
    mut decisions: Query<&mut Decision>,
    services: &SearchServices<'_>,
    mut commands: Commands,
) {
    match start(bindings.get(called.call).ok(), services) {
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
    #[error("the host HTTP runtime is unavailable")]
    Runtime,
}

fn start(
    binding: Option<BoundSearch<'_>>,
    services: &SearchServices<'_>,
) -> Result<Running, SearchStartError> {
    let (search, grant) = binding.ok_or(SearchStartError::NotBound)?;
    let runtime = services
        .runtime
        .as_ref()
        .map(|runtime| runtime.0.clone())
        .or_else(|| tokio::runtime::Handle::try_current().ok())
        .ok_or(SearchStartError::Runtime)?;
    let search = search.clone();
    let store = services.store.as_ref().clone();
    let credentials = services.credentials.as_deref().cloned();
    let providers = services.providers.as_deref().cloned().unwrap_or_default();
    let grant = grant.cloned();
    let task = background::spawn(&runtime, None, async move {
        execute(
            &search.route,
            &search.action.query,
            search.action.limit,
            || {
                authorize_send(
                    &search,
                    &store,
                    credentials.as_ref(),
                    &providers,
                    grant.as_ref(),
                )
            },
        )
        .await
    });
    Ok(Running::answering(
        AsyncComputeTaskPool::get_or_init(TaskPool::default),
        &services.wake,
        async move {
            match task.wait().await {
                Some(Ok(result)) => Outcome::from(ToolResult::success(ToolOutput::json(result))),
                Some(Err(error)) => Outcome::from(ToolResult::failed(error)),
                None => Outcome::from(ToolResult::failed(ToolExecutionError::new(
                    ToolErrorKind::Other,
                    "the web search task stopped",
                ))),
            }
        },
    ))
}

fn authorize_send(
    search: &PreparedSearch,
    store: &Store,
    credentials: Option<&Credentials>,
    providers: &Providers,
    grant: Option<&Approved>,
) -> Result<(), ToolExecutionError> {
    search
        .route
        .binding
        .current(store, credentials, providers)
        .map_err(|error| ToolExecutionError::other(error.to_string()))?;
    let policy = store.policy_repo();
    let policy_failure = |_| ToolExecutionError::other("search policy is unavailable");
    let rules = policy.policy_rules().map_err(policy_failure)?;
    let allowed = match (grant, decide_action(&rules, &search.candidate())) {
        (_, Verdict::Deny { .. }) => false,
        (Some(Approved::Human), _) | (None, Verdict::Allow) => true,
        (
            Some(Approved::Decision {
                texts,
                rules: approved,
            }),
            Verdict::Allow | Verdict::NeedsDecision { .. },
        ) => *approved == rules && *texts == policy.policy_texts().map_err(policy_failure)?,
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(ToolExecutionError::other(
            "the search permission changed before the query was sent",
        ))
    }
}
