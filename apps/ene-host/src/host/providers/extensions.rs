//! Host integration for hook and tool plugin extensions.
//! - `F18` (`Extension::Hook`): invokes `PreTurn`/`PostTurn`/`PreTool`/`PostTool`
//!   hooks in deterministic plugin-ID order, collecting context injections and
//!   blocking verdicts.
//! - `F20` (`Extension::Tool`): discovers plugin-declared tools via `ListTools`,
//!   spawns them as ECS tools, and routes every call through `Stage::Gate`.
mod hooks;

pub(crate) use hooks::run_hooks;

use super::{PluginServices, Providers};
use crate::host::{
    db::Store,
    resources::{HostExternalTool, HostRuntime},
};
use bevy_ecs::prelude::*;
use ene_plugin::{Extension, Operation, PluginToolDefinition, ToolInvokeRequest, ToolInvokeResult};
use rig_core::{
    completion::ToolDefinition,
    effect::{Answered, EffectKind, HandlerDescriptor, family},
    error::{ErrorKind, ErrorReport},
    message::ToolName,
    serve::{Dispatch, ErasedHandler, Reply, Serve},
    tool::{ToolErrorKind, ToolOutput, ToolResult},
};
use rig_ecs::prelude::*;
use serde_json::Value;
use std::collections::HashSet;

/// Aggregated result from invoking all installed `Extension::Hook` plugins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HookOutcome {
    pub(crate) blocked: Option<String>,
    pub(crate) injected_context: Vec<String>,
}

struct PluginToolHandler {
    plugin_id: String,
    definition: PluginToolDefinition,
    providers: Providers,
    store: Store,
    runtime: tokio::runtime::Handle,
}

impl Serve for PluginToolHandler {
    type Family = family::Tool;

    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor::tool(
            &self.definition.name,
            self.definition.description.clone(),
            self.definition.parameters.clone(),
        )
    }

    async fn serve(&self, kind: EffectKind, _: Dispatch) -> Reply {
        let EffectKind::ToolCall { args, .. } = kind else {
            return Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::HandlerUnavailable,
                "plugin tool received non-tool effect",
            )));
        };
        let arguments = match serde_json::from_str::<Value>(&args) {
            Ok(v) => v,
            Err(error) => {
                return Reply::Outcome(Err(ErrorReport::new(
                    ErrorKind::Tool(ToolErrorKind::InvalidArgs),
                    format!("invalid tool arguments JSON: {error}"),
                )));
            }
        };
        let plugin_id = self.plugin_id.clone();
        let tool_name = self.definition.name.clone();
        let providers = self.providers.clone();
        let store = self.store.clone();
        let joined = self
            .runtime
            .spawn(async move {
                let running = providers
                    .start(&plugin_id, Extension::Tool)
                    .await
                    .map_err(|e| e.to_string())?;
                let services = PluginServices::new(plugin_id, store);
                let raw = running
                    .invoke(
                        Operation::InvokeTool {
                            request: ToolInvokeRequest {
                                name: tool_name,
                                arguments,
                            },
                        },
                        services,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                serde_json::from_value::<ToolInvokeResult>(raw)
                    .map_err(|e| format!("invalid plugin tool result: {e}"))
            })
            .await;
        match joined {
            Ok(Ok(result)) if result.is_error => Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::Tool(ToolErrorKind::Other),
                result.output.to_string(),
            ))),
            Ok(Ok(result)) => {
                let text = match result.output {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                Reply::Outcome(Ok(Answered::ToolResult {
                    result: ToolResult::success(ToolOutput::text(text)),
                }))
            }
            Ok(Err(message)) => Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::Tool(ToolErrorKind::Other),
                message,
            ))),
            Err(_) => Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::HandlerUnavailable,
                "plugin tool task cancelled",
            ))),
        }
    }
}

/// Discovers tool definitions from all installed `Extension::Tool` plugins (`F20`).
pub(crate) async fn discover_plugin_tools(
    providers: &Providers,
    store: &Store,
) -> Vec<(String, PluginToolDefinition)> {
    let mut discovered = Vec::new();
    let mut seen_names = HashSet::new();
    for plugin_id in providers.with_extension(Extension::Tool) {
        let mut tools =
            discover_tools_from_plugin(providers, store, &plugin_id, &mut seen_names).await;
        discovered.append(&mut tools);
    }
    discovered
}

async fn discover_tools_from_plugin(
    providers: &Providers,
    store: &Store,
    plugin_id: &str,
    seen_names: &mut HashSet<String>,
) -> Vec<(String, PluginToolDefinition)> {
    let Ok(running) = providers.start(plugin_id, Extension::Tool).await.map_err(
        |error| tracing::warn!(plugin = %plugin_id, %error, "tool plugin failed to start"),
    ) else {
        return Vec::new();
    };
    let services = PluginServices::new(plugin_id.to_owned(), store.clone());
    let Ok(raw) = running
        .invoke(Operation::ListTools, services)
        .await
        .map_err(
            |error| tracing::warn!(plugin = %plugin_id, %error, "tool plugin ListTools failed"),
        )
    else {
        return Vec::new();
    };
    filter_plugin_tool_defs(plugin_id, raw, seen_names)
}

fn filter_plugin_tool_defs(
    plugin_id: &str,
    raw: Value,
    seen_names: &mut HashSet<String>,
) -> Vec<(String, PluginToolDefinition)> {
    let Ok(defs) = serde_json::from_value::<Vec<PluginToolDefinition>>(raw).map_err(|error| {
        tracing::warn!(plugin = %plugin_id, %error, "tool plugin returned invalid ListTools payload");
    }) else {
        return Vec::new();
    };
    defs.into_iter()
        .filter(|def| {
            let valid = !def.name.trim().is_empty()
                && !ene_rpc::BUILTIN_TOOLS.contains(&def.name.as_str())
                && seen_names.insert(def.name.clone());
            if !valid {
                tracing::warn!(
                    plugin = %plugin_id,
                    tool = %def.name,
                    "skipping invalid or colliding plugin tool name"
                );
            }
            valid
        })
        .map(|def| (plugin_id.to_owned(), def))
        .collect()
}

/// Spawns discovered plugin tools as ECS tool entities and refreshes agent `ToolAccess`.
pub(crate) fn register_plugin_tools(world: &mut World, tools: Vec<(String, PluginToolDefinition)>) {
    let Some(runtime) = world
        .get_resource::<HostRuntime>()
        .map(|r| r.0.clone())
        .or_else(|| tokio::runtime::Handle::try_current().ok())
    else {
        return;
    };
    let providers = world
        .get_resource::<Providers>()
        .cloned()
        .unwrap_or_default();
    let store = world.resource::<Store>().clone();
    // Despawn any stale plugin tools before spawning the refreshed set.
    let existing: Vec<_> = world
        .query::<(Entity, &HostExternalTool)>()
        .iter(world)
        .filter(|(_, owner)| matches!(owner, HostExternalTool::Plugin(_)))
        .map(|(entity, _)| entity)
        .collect();
    let mut changed = !existing.is_empty();
    for entity in existing {
        let _despawned = world.try_despawn(entity);
    }
    for (plugin_id, def) in tools {
        let Ok(name) = ToolName::new(def.name.clone()) else {
            continue;
        };
        let tool_def = ToolDefinition {
            name,
            description: def.description.clone(),
            parameters: def.parameters.clone(),
            annotations: rig_core::completion::ToolAnnotations {
                read_only_hint: Some(def.read_only),
                ..Default::default()
            },
        };
        let handler = ErasedHandler::new(PluginToolHandler {
            plugin_id: plugin_id.clone(),
            definition: def,
            providers: providers.clone(),
            store: store.clone(),
            runtime: runtime.clone(),
        });
        if let Some(entity) = world.spawn_tool(tool_def, handler, ToolOptions::default()) {
            world
                .entity_mut(entity)
                .insert(HostExternalTool::Plugin(plugin_id));
            changed = true;
        }
    }
    if changed {
        crate::host::tools::sync::sync_access(world);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::providers::{PLUGINS_DIR, Providers, character::dispatch_character_event};
    use crate::host::tests::Harness;
    use ene_plugin::{CharacterAction, CharacterEvent, CharacterTarget, HookEvent};
    use std::path::{Path, PathBuf};

    fn reference_wasm() -> PathBuf {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let wasm = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join("release")
            .join("ene_reference_wasm.wasm");
        assert!(
            wasm.exists(),
            "missing {}; run `just build-test-plugins` first",
            wasm.display()
        );
        wasm
    }

    fn write_restricted(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for dir in path.ancestors().take_while(|d| d.is_dir()) {
                if dir.ends_with("plugins") {
                    break;
                }
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn install_reference_extensions(state_root: &Path) -> Providers {
        let plugin_dir = state_root.join(PLUGINS_DIR).join("reference-ext");
        let wasm = std::fs::read(reference_wasm()).unwrap();
        write_restricted(&plugin_dir.join("main.wasm"), &wasm);
        let manifest = serde_json::json!({
            "contract_version": ene_plugin::CONTRACT_VERSION,
            "id": "reference-ext",
            "name": "Reference Extensions",
            "version": "1.0.0",
            "entrypoints": [{
                "id": "main",
                "target": "host",
                "file": "main.wasm",
                "provides": ["character", "hook", "tool"],
                "uses": ["log", "configuration", "state"]
            }]
        });
        write_restricted(
            &plugin_dir.join("manifest.json"),
            manifest.to_string().as_bytes(),
        );
        Providers::load(state_root)
    }

    #[tokio::test]
    async fn t12_plugin_services_config_state_and_character_extension() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let providers = install_reference_extensions(temp.path());

        let target = CharacterTarget {
            device_id: "dev-1".into(),
            character_id: "ene".into(),
        };
        let first = dispatch_character_event(
            &providers,
            &store,
            target.clone(),
            CharacterEvent::Activated,
        )
        .await;
        assert_eq!(first.len(), 1);
        assert!(matches!(
            &first[0],
            CharacterAction::Expression { name, duration_ms: 1500 } if name == "happy"
        ));
        assert_eq!(
            store
                .plugin_repo()
                .plugin_state_read("reference-ext", "visits")
                .unwrap(),
            Some(serde_json::json!(1))
        );
        // Second call reads persisted state from SQLite and increments cleanly.
        let second = dispatch_character_event(
            &providers,
            &store,
            target,
            CharacterEvent::ConversationFinished { text: "bye".into() },
        )
        .await;
        assert_eq!(second.len(), 1);
        assert_eq!(
            store
                .plugin_repo()
                .plugin_state_read("reference-ext", "visits")
                .unwrap(),
            Some(serde_json::json!(2))
        );
        // Isolation check: another plugin ID cannot read `reference-ext`'s state.
        assert_eq!(
            store
                .plugin_repo()
                .plugin_state_read("other-plugin", "visits")
                .unwrap(),
            None
        );

        // Policy-gated ServiceRequest::Tool callback check (`F19`).
        let tool_services = PluginServices::with_tool_callback(
            "reference-ext",
            store.clone(),
            std::sync::Arc::new(|name, args| {
                Box::pin(async move { Ok(serde_json::json!({ "called": name, "args": args })) })
            }),
        );
        let reply = ene_plugin_runtime::instance::Services::handle(
            tool_services.as_ref(),
            ene_plugin::ServiceRequest::Tool {
                name: "ping".into(),
                arguments: serde_json::json!({"ok": true}),
            },
        )
        .await
        .unwrap();
        assert_eq!(reply["called"], "ping");
    }

    #[tokio::test]
    async fn t13_hook_extension_injects_context_and_blocks_tool() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let providers = install_reference_extensions(temp.path());

        store
            .plugin_repo()
            .set_plugin_config(
                "reference-ext",
                &serde_json::json!({ "inject_note": "フックからの追加コンテキスト" }),
            )
            .unwrap();
        let pre = run_hooks(
            &providers,
            &store,
            HookEvent::PreTurn {
                companion_id: "ene".into(),
                query: Some("hello".into()),
            },
        )
        .await;
        assert_eq!(pre.blocked, None);
        assert_eq!(pre.injected_context, vec!["フックからの追加コンテキスト"]);

        let blocked = run_hooks(
            &providers,
            &store,
            HookEvent::PreTool {
                companion_id: "ene".into(),
                tool_name: "reference_echo".into(),
                arguments: serde_json::json!({ "block_me": true }),
            },
        )
        .await;
        assert!(
            blocked
                .blocked
                .as_deref()
                .is_some_and(|r| r.contains("hook blocked")),
            "expected hook to block tool, got {blocked:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn t14_tool_extension_discovers_gates_and_executes_through_permission_pipeline() {
        use crate::host::agent::WorkAgent;
        use rig_core::completion::Message;
        use rig_core::message::{ToolResultContent, UserContent};
        use rig_core::test_utils::MockStreamEvent;

        let mut harness = Harness::new([
            vec![
                MockStreamEvent::tool_call(
                    "call-1",
                    "reference_echo",
                    serde_json::json!({"text": "hello plugin"}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("エコーしたよ"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("完了"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let providers = install_reference_extensions(harness.root.path());
        harness.app.insert_resource(providers.clone());
        harness
            .app
            .insert_resource(HostRuntime(tokio::runtime::Handle::current()));
        let discovered = discover_plugin_tools(&providers, &harness.store()).await;
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].1.name, "reference_echo");
        register_plugin_tools(harness.app.world_mut(), discovered);

        let companion_id = harness.companion.clone();
        crate::host::work::start(
            harness.app.world_mut(),
            &ene_rpc::StartWorkRequest {
                companion_id,
                instruction: "エコーして".into(),
                ..Default::default()
            },
        )
        .unwrap();
        // Without a saved Allow rule, Stage::Gate MUST pause on an approval card.
        harness.run_until(|world| {
            !world
                .resource::<Store>()
                .approval_repo()
                .approvals(crate::host::permission::ApprovalStatus::Pending)
                .unwrap()
                .is_empty()
        });
        let pending_id = harness
            .store()
            .approval_repo()
            .approvals(crate::host::permission::ApprovalStatus::Pending)
            .unwrap()[0]
            .request_id
            .clone();
        crate::host::permission::submit(
            harness.app.world_mut(),
            &pending_id,
            crate::host::permission::ApprovalDecision::Approve,
        )
        .unwrap();

        harness.run_until(|world| {
            world
                .query_filtered::<&Conversation, With<WorkAgent>>()
                .iter(world)
                .any(|conv| {
                    conv.entries().any(|entry| match &entry.message {
                        Message::User { content } => content.iter().any(|item| {
                            matches!(item, UserContent::ToolResult(result)
                            if result.name.as_str() == "reference_echo"
                                && !result.is_error
                                && result.content.iter().any(|block| match block {
                                    ToolResultContent::Text(t) => t.text.contains("hello plugin"),
                                    ToolResultContent::Json { value } => {
                                        value.to_string().contains("hello plugin")
                                    }
                                    _ => false,
                                }))
                        }),
                        _ => false,
                    })
                })
        });

        let world = harness.app.world_mut();
        let agent = world
            .query_filtered::<Entity, With<WorkAgent>>()
            .iter(world)
            .next()
            .unwrap();
        assert!(
            world
                .get::<ToolAccess>(agent)
                .unwrap()
                .allows("reference_echo")
        );
        register_plugin_tools(world, Vec::new());
        assert!(
            !world
                .get::<ToolAccess>(agent)
                .unwrap()
                .allows("reference_echo")
        );
    }
}
