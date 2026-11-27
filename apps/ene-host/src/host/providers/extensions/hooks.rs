//! `F18` (`Extension::Hook`): invokes `PreTurn`/`PostTurn`/`PreTool`/`PostTool`
//! hooks in deterministic plugin-ID order, collecting context injections and
//! blocking verdicts.
use super::HookOutcome;
use crate::host::{
    db::Store,
    providers::{PluginServices, Providers},
};
use ene_plugin::{Extension, HookDecision, HookEvent, Operation};
use std::sync::Arc;

/// Runs all installed `Extension::Hook` plugins in sorted plugin-ID order (`F18`).
pub(crate) async fn run_hooks(
    providers: &Providers,
    store: &Store,
    event: HookEvent,
) -> HookOutcome {
    let mut outcome = HookOutcome::default();
    for plugin_id in providers.with_extension(Extension::Hook) {
        match invoke_hook_plugin(providers, store, &plugin_id, &event).await {
            Some(HookDecision::Continue) | None => {}
            Some(HookDecision::Block { reason }) => {
                outcome.blocked = Some(if reason.trim().is_empty() {
                    format!("blocked by hook plugin `{plugin_id}`")
                } else {
                    reason
                });
                return outcome;
            }
            Some(HookDecision::InjectContext { text }) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    outcome.injected_context.push(trimmed.to_owned());
                }
            }
        }
    }
    outcome
}

async fn invoke_hook_plugin(
    providers: &Providers,
    store: &Store,
    plugin_id: &str,
    event: &HookEvent,
) -> Option<HookDecision> {
    let running = providers
        .start(plugin_id, Extension::Hook)
        .await
        .map_err(|error| tracing::warn!(plugin = %plugin_id, %error, "hook plugin failed to start"))
        .ok()?;
    let services = PluginServices::new(Arc::clone(&running), store.clone());
    let raw = running
        .invoke(
            Operation::Hook {
                event: event.clone(),
            },
            services,
        )
        .await
        .map_err(
            |error| tracing::warn!(plugin = %plugin_id, %error, "hook plugin invocation failed"),
        )
        .ok()?;
    serde_json::from_value::<HookDecision>(raw)
        .map_err(|error| {
            tracing::warn!(plugin = %plugin_id, %error, "hook plugin returned invalid decision")
        })
        .ok()
}
