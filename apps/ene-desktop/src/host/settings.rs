use super::*;
use ene_api_types::host::*;
pub(super) enum ActionResult {
    Done(String, bool),
    History(Page<Message>),
    Models(String, Vec<ene_api_types::provider::Model>),
}
fn connection<'a>(snapshot: &'a Snapshot, id: &str) -> Result<&'a Connection, Error> {
    snapshot
        .settings
        .connections
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| Error::Api("先に接続を保存してください".into()))
}
pub(super) async fn action(
    http: Http,
    snapshot: Snapshot,
    command: Command,
) -> Result<ActionResult, Error> {
    let c = snapshot
        .companions
        .first()
        .ok_or_else(|| Error::Api("コンパニオンがありません".into()))?;
    let key = Uuid::new_v4().to_string();
    let committed = matches!(
        &command,
        Command::Configure { .. } | Command::Companion { .. } | Command::Permission { .. }
    );
    let mut feedback = "保存しました。";
    match command {
        Command::Configure {
            provider,
            endpoint,
            model,
            options,
        } => {
            let descriptors = http.providers().await?;
            let descriptor = descriptors
                .iter()
                .find(|p| p.id == provider && p.state == "ready")
                .ok_or_else(|| Error::Api("プロバイダーを有効化してください".into()))?;
            let mut merged = descriptor.options.as_object().cloned().unwrap_or_default();
            merged.extend(options.as_object().cloned().ok_or(Error::Transport)?);
            let options = Value::Object(merged);
            let mut connections: Vec<_> = snapshot
                .settings
                .connections
                .iter()
                .map(|p| ConnectionInput {
                    id: Some(p.id.clone()),
                    kind: p.kind.clone(),
                    adapter: p.adapter.clone(),
                    endpoint: p.endpoint.clone(),
                    options: p.options.clone(),
                })
                .collect();
            if let Some(p) = connections.iter_mut().find(|p| p.adapter == provider) {
                p.endpoint = endpoint;
                p.options = options;
            } else {
                connections.push(ConnectionInput {
                    id: None,
                    kind: "llm".into(),
                    adapter: provider.clone(),
                    endpoint,
                    options,
                });
            }
            let saved = http
                .patch_settings(&SettingsPatch {
                    revision: snapshot.settings.revision,
                    connections,
                })
                .await?;
            let p = saved
                .connections
                .iter()
                .find(|p| p.adapter == provider)
                .ok_or(Error::Transport)?;
            if !model.trim().is_empty() {
                let assignment = Some(ModelAssignment {
                    connection_id: p.id.clone(),
                    model_id: model,
                });
                http.patch_companion(
                    &c.id,
                    &CompanionPatch {
                        revision: c.revision,
                        name: None,
                        mind: None,
                        model_assignments: Some(
                            [
                                ("companion".into(), assignment.clone()),
                                ("task".into(), assignment),
                            ]
                            .into(),
                        ),
                    },
                )
                .await?;
            }
        }
        Command::ReloadProviders => {
            http.reload_providers().await?;
        }
        Command::EnableProvider { provider, enabled } => {
            http.enable_provider(&provider, enabled).await?;
        }
        Command::Models(id) => {
            return Ok(ActionResult::Models(
                id.clone(),
                http.models(&connection(&snapshot, &id)?.id, Some(&c.id))
                    .await?,
            ));
        }
        Command::Companion { name, mind } => {
            http.patch_companion(
                &c.id,
                &CompanionPatch {
                    revision: c.revision,
                    name: Some(name),
                    mind: Some(mind),
                    model_assignments: None,
                },
            )
            .await?;
        }
        Command::Permission {
            connection: id,
            conversation,
            check,
        } => {
            let p = connection(&snapshot, &id)?;
            let providers = http.providers().await?;
            let destinations = &providers
                .iter()
                .find(|d| d.id == p.adapter)
                .ok_or(Error::Transport)?
                .destinations;
            for (purpose, enabled, data) in [
                ("conversation", conversation, "conversation"),
                ("connection_check", check, "test_input"),
            ] {
                let grants: Vec<_> = snapshot
                    .grants
                    .iter()
                    .filter(|g| {
                        g.companion_id == c.id
                            && g.connection_id == p.id
                            && g.purpose == purpose
                            && g.status == "active"
                    })
                    .collect();
                if enabled && grants.is_empty() {
                    http.create_grant(
                        &GrantInput {
                            companion_id: c.id.clone(),
                            kind: "data_transfer".into(),
                            connection_id: p.id.clone(),
                            destinations: destinations.clone(),
                            data_kinds: if purpose == "connection_check" {
                                vec![data.into(), "metadata".into()]
                            } else {
                                vec![data.into()]
                            },
                            purpose: purpose.into(),
                        },
                        &IdempotencyKey::new(format!("{key}-{purpose}")).map_err(Error::from)?,
                    )
                    .await?;
                }
                if !enabled {
                    for g in grants {
                        http.revoke_grant(&g.id).await?;
                    }
                }
            }
        }
        Command::History => {
            let Some(cursor) = snapshot.message_cursors.get(&c.id).and_then(|v| v.as_ref()) else {
                return Ok(ActionResult::Done(
                    "過去の履歴はすべて表示しています".into(),
                    false,
                ));
            };
            return Ok(ActionResult::History(
                http.messages(&c.id, Some(cursor), None).await?,
            ));
        }
        command => match command {
            Command::Credential(id, secret) => {
                let pid = &connection(&snapshot, &id)?.id;
                let result = http
                    .register_credentials(
                        pid,
                        &CredentialInput {
                            secret: Some(secret),
                            source: None,
                        },
                    )
                    .await?;
                feedback = if result.credential_state == "stored" {
                    "APIキーを資格情報ストアに保存しました"
                } else {
                    "APIキーは今回のホスト起動中だけ保持します"
                };
            }
            Command::EnvironmentCredential(id) => {
                let pid = &connection(&snapshot, &id)?.id;
                http.register_credentials(
                    pid,
                    &CredentialInput {
                        secret: None,
                        source: Some("environment".into()),
                    },
                )
                .await?;
                feedback = "ホストの環境変数からAPIキーを登録しました";
            }
            Command::RevokeCredential(id) => {
                let pid = &connection(&snapshot, &id)?.id;
                http.revoke_credentials(pid).await?;
                feedback = "資格情報を解除し、生成を停止しました";
            }
            Command::Check(id) => {
                let pid = &connection(&snapshot, &id)?.id;
                http.check(
                    pid,
                    &CheckInput {
                        companion_id: c.id.clone(),
                        purpose: "companion".into(),
                        model_id: c
                            .model_assignments
                            .companion
                            .as_ref()
                            .filter(|a| a.connection_id == *pid)
                            .map(|a| a.model_id.clone()),
                    },
                    &IdempotencyKey::new(key).map_err(Error::from)?,
                )
                .await?;
                feedback = "実モデルの逐次応答を確認しました";
            }
            _ => unreachable!(),
        },
    }
    Ok(ActionResult::Done(feedback.into(), committed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::Query,
        routing::{get, patch},
    };
    use std::collections::HashMap;

    #[tokio::test]
    async fn connection_without_initial_model_can_discover_metadata_before_assignment() {
        let saved = serde_json::json!({"revision":2,"connections":[
            {"id":"new","kind":"llm","adapter":"dynamic","endpoint":"https://example.com/v1",
             "options":{},"credential_state":"missing","capabilities":null,"last_check":null}]});
        let reply = saved.clone();
        let app = Router::new()
            .route("/v1/providers", get(|| async { Json(serde_json::json!([{
                "id":"dynamic","name":"Dynamic","kind":"plugin","endpoint":"https://example.com/v1",
                "options":{},"settings":[],"requires_credentials":true,"environment_variable":null,
                "destinations":["https://example.com"],"state":"ready","error":null,"revision":"v1",
                "capabilities":{"streaming_text":true,"model_list":true}
            }])) }))
            .route("/v1/settings", patch(move |Json(input): Json<SettingsPatch>| async move {
                assert_eq!(input.connections[0].adapter, "dynamic");
                Json(reply)
            }))
            .route("/v1/connections/new/models", get(|Query(query): Query<HashMap<String,String>>| async move {
                assert_eq!(query.get("companion_id").map(String::as_str), Some("c"));
                Json(serde_json::json!([
                    {"id":"new-model","name":"New","metadata":{"source":"https://example.com/v1/models",
                        "scope":"route","context_window":65536,"input_modalities":["text","image"]}},
                    {"id":"legacy","name":"Legacy"}
                ]))
            }));
        // No companion PATCH route: saving the connection must not require a model.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Http::new(&RuntimeConnection {
            endpoint: format!("http://{}/v1", listener.local_addr().unwrap()),
            token: "fixture".into(),
        })
        .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut snapshot: Snapshot = serde_json::from_value(serde_json::json!({
            "host_id":"h","environment_id":"e","capabilities":{},"cursor":{"epoch":"e","seq":0},
            "companions":[{"id":"c","name":"ene","mind":"","body":null,"voice":null,
                "model_assignments":{},"availability":{},"revision":1}],
            "messages":[],"message_cursors":{},"tasks":[],"questions":[],
            "settings":{"revision":1,"connections":[]},"grants":[]
        }))
        .unwrap();
        assert!(matches!(
            action(
                http.clone(),
                snapshot.clone(),
                Command::Configure {
                    provider: "dynamic".into(),
                    endpoint: "https://example.com/v1".into(),
                    model: String::new(),
                    options: serde_json::json!({}),
                }
            )
            .await
            .unwrap(),
            ActionResult::Done(_, true)
        ));
        snapshot.settings = serde_json::from_value(saved).unwrap();
        let ActionResult::Models(id, models) =
            action(http, snapshot, Command::Models("new".into()))
                .await
                .unwrap()
        else {
            panic!("missing catalog")
        };
        assert_eq!(id, "new");
        assert_eq!(models[0].id, "new-model");
        server.abort();
    }
}
