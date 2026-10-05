use super::transport::Error;
use super::{Command, CredentialEdit, Http, IdempotencyKey};
use ene_api_types::host::{
    CheckInput, CompanionPatch, Connection, ConnectionInput, CredentialInput, GrantInput, Message,
    ModelAssignment, Page, SettingsPatch, Snapshot,
};
use serde_json::Value;
use uuid::Uuid;

pub(super) enum ActionResult {
    Done(String, bool, Option<String>),
    History(String, Page<Message>),
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
    companion: String,
    command: Command,
) -> Result<ActionResult, Error> {
    let c = snapshot
        .companions
        .iter()
        .find(|c| c.id == companion)
        .ok_or_else(|| Error::Api("コンパニオンがありません".into()))?;
    let key = Uuid::new_v4().to_string();
    let committed = matches!(
        &command,
        Command::Configure { .. }
            | Command::Companion { .. }
            | Command::Permission { .. }
            | Command::RevokePermissions(_)
    );
    let mut feedback = "保存しました。";
    let mut selected_connection = None;
    match command {
        Command::Configure {
            settings_revision,
            companion_revision,
            connection_id,
            creation_id,
            provider,
            endpoint,
            model,
            options,
            update_connection,
            credential,
        } => {
            let target = connection_id
                .clone()
                .or(creation_id)
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let connection_id = if update_connection {
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
                        create: false,
                        kind: p.kind.clone(),
                        adapter: p.adapter.clone(),
                        endpoint: p.endpoint.clone(),
                        options: p.options.clone(),
                    })
                    .collect();
                if let Some(p) = connections
                    .iter_mut()
                    .find(|p| p.id.as_deref() == Some(&target))
                {
                    p.adapter = provider;
                    p.endpoint = endpoint;
                    p.options = options;
                } else if connection_id.is_none() {
                    connections.push(ConnectionInput {
                        id: Some(target.clone()),
                        create: true,
                        kind: "llm".into(),
                        adapter: provider,
                        endpoint,
                        options,
                    });
                } else {
                    return Err(Error::Transport);
                }
                let saved = http
                    .patch_settings(&SettingsPatch {
                        revision: settings_revision,
                        connections,
                    })
                    .await?;
                saved
                    .connections
                    .iter()
                    .find(|p| p.id == target)
                    .ok_or(Error::Transport)?
                    .id
                    .clone()
            } else {
                connection(&snapshot, &target)?.id.clone()
            };
            selected_connection = Some(connection_id.clone());
            // Preserve the confirmed ID even when a later save step fails.
            let remaining = async {
                // A previous partial/uncertain save may already have installed
                // this assignment. Do not rewrite it with a newer revision.
                if update_connection
                    && !model.trim().is_empty()
                    && c.model_assignments
                        .companion
                        .as_ref()
                        .is_none_or(|a| a.connection_id != connection_id || a.model_id != model)
                {
                    http.patch_companion(
                        &c.id,
                        &CompanionPatch {
                            revision: companion_revision,
                            name: None,
                            mind: None,
                            model_assignments: Some(
                                [(
                                    "companion".into(),
                                    Some(ModelAssignment {
                                        connection_id: connection_id.clone(),
                                        model_id: model,
                                    }),
                                )]
                                .into(),
                            ),
                        },
                    )
                    .await?;
                }
                if let Some(credential) = credential {
                    match credential {
                        CredentialEdit::Secret(secret) => {
                            let saved = http
                                .register_credentials(
                                    &connection_id,
                                    &CredentialInput {
                                        secret: Some(secret),
                                        source: None,
                                    },
                                )
                                .await?;
                            if saved.credential_state != "stored" {
                                feedback =
                                    "保存しました。APIキーは今回のホスト起動中のみ有効です。";
                            }
                        }
                        CredentialEdit::Environment => {
                            http.register_credentials(
                                &connection_id,
                                &CredentialInput {
                                    secret: None,
                                    source: Some("environment".into()),
                                },
                            )
                            .await?;
                        }
                        CredentialEdit::Revoke => {
                            http.revoke_credentials(&connection_id).await?;
                        }
                    }
                }
                Ok::<_, Error>(())
            }
            .await;
            remaining.map_err(|error| Error::PartialConfiguration {
                connection: connection_id,
                error: Box::new(error),
            })?;
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
        Command::Companion {
            revision,
            name,
            mind,
        } => {
            http.patch_companion(
                &c.id,
                &CompanionPatch {
                    revision,
                    name: Some(name),
                    mind: Some(mind),
                    model_assignments: None,
                },
            )
            .await?;
        }
        Command::RevokePermissions(id) => {
            connection(&snapshot, &id)?;
            for grant in http
                .grants(Some(&c.id))
                .await?
                .into_iter()
                .filter(|g| g.connection_id == id && g.status == "active")
            {
                http.revoke_grant(&grant.id).await?;
            }
            feedback = "送信許可を解除しました";
        }
        Command::Permission {
            consent,
            destinations,
            connection: id,
            conversation,
            check,
        } => {
            let p = connection(&snapshot, &id)?;

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
                            consent: consent.clone(),
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
                    None,
                ));
            };
            return Ok(ActionResult::History(
                c.id.clone(),
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
    Ok(ActionResult::Done(
        feedback.into(),
        committed,
        selected_connection,
    ))
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use ene_api_types::host::RuntimeConnection;
    #[tokio::test]
    async fn configure_updates_selected_connection_and_only_conversation_assignment() {
        let value = serde_json::json!({
            "host_id":"h","environment_id":"e","capabilities":{},"cursor":{"epoch":"e","seq":0},
            "companions":[{"id":"c","name":"ene","mind":"","body":null,"voice":null,
                "model_assignments":{"task":{"connection_id":"first","model_id":"task-model"}},"availability":{},"revision":1}],
            "messages":[],"message_cursors":{},"tasks":[],"questions":[],"grants":[],
            "settings":{"revision":1,"connections":[
                {"id":"first","kind":"llm","adapter":"dynamic","endpoint":"https://first.test/v1","options":{},"credential_state":"stored","capabilities":null,"last_check":null},
                {"id":"selected","kind":"llm","adapter":"dynamic","endpoint":"https://selected.test/v1","options":{},"credential_state":"missing","capabilities":null,"last_check":null}]}
        });
        let snapshot: Snapshot = serde_json::from_value(value).unwrap();
        let mut reply = snapshot.settings.clone();
        reply.revision += 1;
        reply.connections[1].endpoint = "https://updated.test/v1".into();
        let companion = snapshot.companions[0].clone();
        let patched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = patched.clone();
        let app = axum::Router::new()
            .route("/v1/providers", axum::routing::get(|| async { axum::Json(serde_json::json!([{
                "id":"dynamic","name":"Dynamic","kind":"plugin","endpoint":"https://selected.test/v1","options":{},"settings":[],
                "requires_credentials":false,"environment_variable":null,"destinations":["https://selected.test"],"state":"ready","error":null,"revision":"v1","capabilities":{"streaming_text":true,"model_list":true}}])) }))
            .route("/v1/settings",axum::routing::patch(move |axum::Json(input):axum::Json<SettingsPatch>| async move {
                assert_eq!(input.connections[0].endpoint,"https://first.test/v1");
                assert_eq!(input.connections[1].id.as_deref(),Some("selected"));
                assert_eq!(input.connections[1].endpoint,"https://updated.test/v1");
                axum::Json(reply)
            }))
            .route("/v1/companions/c",axum::routing::patch(move |axum::Json(input):axum::Json<CompanionPatch>| {
                let observed = observed.clone(); let mut companion = companion.clone();
                async move {
                    let assignments = input.model_assignments.unwrap();
                    assert_eq!(assignments.len(),1);
                    assert_eq!(assignments["companion"].as_ref().unwrap().connection_id,"selected");
                    companion.model_assignments.companion = assignments["companion"].clone();
                    assert_eq!(companion.model_assignments.task.as_ref().unwrap().model_id,"task-model");
                    observed.store(true,std::sync::atomic::Ordering::SeqCst);
                    axum::Json(companion)
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Http::new(&RuntimeConnection {
            endpoint: format!("http://{}/v1", listener.local_addr().unwrap()),
            token: "fixture".into(),
        })
        .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        action(
            http,
            snapshot,
            "c".into(),
            Command::Configure {
                settings_revision: 1,
                companion_revision: 1,
                connection_id: Some("selected".into()),
                creation_id: None,
                provider: "dynamic".into(),
                endpoint: "https://updated.test/v1".into(),
                model: "chat-model".into(),
                options: serde_json::json!({}),
                update_connection: true,
                credential: None,
            },
        )
        .await
        .unwrap();
        assert!(patched.load(std::sync::atomic::Ordering::SeqCst));
        server.abort();
    }

    #[tokio::test]
    async fn partial_and_uncertain_configuration_retries_keep_one_connection() {
        use std::sync::{Arc, Mutex};
        for lose_response in [false, true] {
            let mut snapshot:Snapshot=serde_json::from_value(serde_json::json!({
                "host_id":"h","environment_id":"e","capabilities":{},"cursor":{"epoch":"e","seq":0},
                "companions":[{"id":"c","name":"ene","mind":"","body":null,"voice":null,"model_assignments":{},"availability":{},"revision":1}],
                "messages":[],"message_cursors":{},"tasks":[],"questions":[],"grants":[],
                "settings":{"revision":1,"connections":[]}
            })).unwrap();
            let stored = Arc::new(Mutex::new(snapshot.settings.clone()));
            let saved = stored.clone();
            let calls = Arc::new(Mutex::new(0));
            let observed = calls.clone();
            let app=axum::Router::new()
                .route("/v1/providers",axum::routing::get(||async {axum::Json(serde_json::json!([{
                    "id":"dynamic","name":"Dynamic","kind":"plugin","endpoint":"https://example.test/v1","options":{},"settings":[],
                    "requires_credentials":true,"environment_variable":null,"destinations":["https://example.test"],"state":"ready","error":null,"revision":"v1","capabilities":{"streaming_text":true,"model_list":true}
                }]))}))
                .route("/v1/settings",axum::routing::patch(move |axum::Json(input):axum::Json<SettingsPatch>| {
                    let saved=saved.clone();let observed=observed.clone();
                    async move {
                        let mut calls=observed.lock().unwrap();*calls+=1;
                        let mut db=saved.lock().unwrap();
                        if input.revision==db.revision {
                            db.revision+=1;
                            db.connections=input.connections.into_iter().map(|c|Connection {
                                id:c.id.expect("caller retains creation identity"),kind:c.kind,adapter:c.adapter,
                                endpoint:c.endpoint,options:c.options,credential_state:"missing".into(),capabilities:None,last_check:None,
                            }).collect();
                        } else {
                            assert_eq!(input.connections[0].id.as_deref(),Some(db.connections[0].id.as_str()));
                        }
                        if lose_response && *calls==1 {
                            (axum::http::StatusCode::SERVICE_UNAVAILABLE,axum::Json(serde_json::json!({"error":{"code":"unavailable","message":"response lost","details":{}}})))
                        } else {
                            (axum::http::StatusCode::OK,axum::Json(serde_json::to_value(&*db).unwrap()))
                        }
                    }
                }))
                .route("/v1/credentials/{id}",axum::routing::put(move ||async move {
                    if lose_response {
                        (axum::http::StatusCode::OK,axum::Json(serde_json::json!({"credential_state":"stored"})))
                    } else {
                        (axum::http::StatusCode::BAD_REQUEST,axum::Json(serde_json::json!({"error":{"code":"invalid_request","message":"invalid key","details":{}}})))
                    }
                }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let http = Http::new(&RuntimeConnection {
                endpoint: format!("http://{}/v1", listener.local_addr().unwrap()),
                token: "fixture".into(),
            })
            .unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let creation = Uuid::new_v4().to_string();
            let mut selected = None;
            for attempt in 0..2 {
                let result = action(
                    http.clone(),
                    snapshot.clone(),
                    "c".into(),
                    Command::Configure {
                        settings_revision: 1,
                        companion_revision: 1,
                        connection_id: selected.clone(),
                        creation_id: Some(creation.clone()),
                        provider: "dynamic".into(),
                        endpoint: "https://example.test/v1".into(),
                        model: String::new(),
                        options: serde_json::json!({}),
                        update_connection: true,
                        credential: Some(CredentialEdit::Secret("bad\nkey".into())),
                    },
                )
                .await;
                if lose_response {
                    if attempt == 0 {
                        assert!(result.is_err());
                    } else {
                        assert!(result.is_ok());
                    }
                    // Deliberately retry the old snapshot when the write's response was lost.
                } else {
                    let Err(Error::PartialConfiguration { connection, .. }) = result else {
                        panic!("confirmed identity must survive later failure")
                    };
                    assert_eq!(connection, creation);
                    selected = Some(connection);
                    snapshot.settings = stored.lock().unwrap().clone();
                }
            }
            assert_eq!(stored.lock().unwrap().connections.len(), 1);
            assert_eq!(stored.lock().unwrap().connections[0].id, creation);
            server.abort();
        }
    }
}
