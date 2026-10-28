use super::transport::Error;
use super::{Command, CredentialEdit, Http};
use connectrpc::ConnectError as ClientError;
use ene_rpc::{
    AssignmentPatch, CheckRequest, CheckResult, Connection, ConnectionInput, ConnectionInputs,
    CreateGrantRequest, CredentialRequest, JsonDocument, ModelAssignment, Snapshot,
    UpdateCompanionRequest, UpdateSettingsRequest,
};
use std::time::Duration;
#[cfg(test)]
use uuid::Uuid;

pub(super) enum ActionResult {
    Done(String, bool, Option<String>),
    Models(String, Vec<ene_rpc::Model>),
    Availability(ene_rpc::Availability),
}
fn connection<'a>(snapshot: &'a Snapshot, id: &str) -> Result<&'a Connection, Error> {
    snapshot
        .settings
        .connections
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| Error::Api("先に接続を保存してください".into()))
}
pub(super) async fn action_with_key(
    http: Http,
    snapshot: Snapshot,
    companion: String,
    command: Command,
    key: String,
) -> Result<ActionResult, Error> {
    let c = snapshot
        .companions
        .iter()
        .find(|c| c.id == companion)
        .ok_or_else(|| Error::Api("コンパニオンがありません".into()))?;
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
            let create = connection_id.is_none();
            let target = connection_id.or(creation_id).ok_or(Error::Transport)?;
            let mut connection_confirmed = false;
            let credential_state = async {
                if target.is_empty() {
                    return Err(ClientError::invalid_argument("invalid connection"));
                }
                if update_connection {
                    let providers = http
                        .list_providers_with_options(
                            ene_rpc::Empty::default(),
                            crate::infrastructure::host::call_options(5),
                        )
                        .await?
                        .into_owned()
                        .items;
                    let descriptor = providers
                        .iter()
                        .find(|candidate| candidate.id == provider && candidate.state == "ready")
                        .ok_or(ClientError::internal("invalid API response"))?;
                    let defaults: serde_json::Value = if descriptor.options.json.is_empty() {
                        serde_json::json!({})
                    } else {
                        serde_json::from_str(&descriptor.options.json)
                            .map_err(|_| ClientError::internal("invalid API response"))?
                    };
                    let overrides: serde_json::Value = serde_json::from_str(&options.json)
                        .map_err(|_| ClientError::internal("invalid API response"))?;
                    let mut options = defaults.as_object().cloned().unwrap_or_default();
                    options.extend(
                        overrides
                            .as_object()
                            .cloned()
                            .ok_or(ClientError::internal("invalid API response"))?,
                    );
                    let options = JsonDocument {
                        json: serde_json::Value::Object(options).to_string(),
                        ..Default::default()
                    };
                    let mut connections: Vec<_> = snapshot
                        .settings
                        .connections
                        .iter()
                        .map(|connection| ConnectionInput {
                            id: Some(connection.id.clone()),
                            create: false,
                            kind: connection.kind.clone(),
                            adapter: connection.adapter.clone(),
                            endpoint: connection.endpoint.clone(),
                            options: connection.options.clone(),
                            ..Default::default()
                        })
                        .collect();
                    if let Some(connection) = connections
                        .iter_mut()
                        .find(|connection| connection.id.as_deref() == Some(&target))
                    {
                        connection.adapter = provider;
                        connection.endpoint = endpoint;
                        connection.options = options.into();
                    } else if create {
                        connections.push(ConnectionInput {
                            id: Some(target.clone()),
                            create: true,
                            kind: "llm".into(),
                            adapter: provider,
                            endpoint,
                            options: options.into(),
                            ..Default::default()
                        });
                    } else {
                        return Err(ClientError::internal("invalid API response"));
                    }
                    let saved = http
                        .update_settings_with_options(
                            UpdateSettingsRequest {
                                revision: settings_revision,
                                connections: ConnectionInputs {
                                    items: connections,
                                    ..Default::default()
                                }
                                .into(),
                                ..Default::default()
                            },
                            crate::infrastructure::host::call_options(10),
                        )
                        .await?
                        .into_owned();
                    if !saved
                        .connections
                        .iter()
                        .any(|connection| connection.id == target)
                    {
                        return Err(ClientError::internal("invalid API response"));
                    }
                } else if !snapshot
                    .settings
                    .connections
                    .iter()
                    .any(|connection| connection.id == target)
                {
                    return Err(ClientError::internal("invalid API response"));
                }
                connection_confirmed = true;
                if !model.trim().is_empty()
                    && c.model_assignments
                        .companion
                        .as_option()
                        .is_none_or(|assignment| {
                            assignment.connection_id != target || assignment.model_id != model
                        })
                {
                    http.update_companion_with_options(
                        UpdateCompanionRequest {
                            id: c.id.clone(),
                            revision: companion_revision,
                            companion_assignment: AssignmentPatch {
                                change: Some(ene_rpc::assignment_patch::Change::Set(Box::new(
                                    ModelAssignment {
                                        connection_id: target.clone(),
                                        model_id: model,
                                        ..Default::default()
                                    },
                                ))),
                                ..Default::default()
                            }
                            .into(),
                            ..Default::default()
                        },
                        crate::infrastructure::host::call_options(10),
                    )
                    .await?;
                }
                let credential_state = match credential {
                    Some(CredentialEdit::Secret(secret)) => Some(
                        http.register_credentials_with_options(
                            CredentialRequest {
                                id: target.clone(),
                                secret: Some(secret),
                                ..Default::default()
                            },
                            crate::infrastructure::host::call_options(10),
                        )
                        .await?
                        .into_owned()
                        .credential_state,
                    ),
                    Some(CredentialEdit::Environment) => Some(
                        http.register_credentials_with_options(
                            CredentialRequest {
                                id: target.clone(),
                                source: Some("environment".into()),
                                ..Default::default()
                            },
                            crate::infrastructure::host::call_options(10),
                        )
                        .await?
                        .into_owned()
                        .credential_state,
                    ),
                    Some(CredentialEdit::Revoke) => {
                        http.revoke_credentials_with_options(
                            ene_rpc::Id {
                                id: target.clone(),
                                ..Default::default()
                            },
                            crate::infrastructure::host::call_options(10),
                        )
                        .await?;
                        Some("missing".into())
                    }
                    None => None,
                };
                Ok::<_, ClientError>(credential_state)
            }
            .await
            .map_err(|error| {
                let error = Error::from(error);
                if connection_confirmed {
                    Error::PartialConfiguration {
                        connection: target.clone(),
                        error: Box::new(error),
                    }
                } else {
                    error
                }
            })?;
            selected_connection = Some(target);
            if credential_state.as_deref() == Some("session_only") {
                feedback = "保存しました。APIキーは今回のホスト起動中のみ有効です。";
            }
        }
        Command::ReloadProviders => {
            http.reload_providers_with_options(
                ene_rpc::Empty::default(),
                crate::infrastructure::host::call_options(10),
            )
            .await?;
        }
        Command::EnableProvider { provider, enabled } => {
            http.enable_provider_with_options(
                ene_rpc::EnableProviderRequest {
                    id: provider.clone(),
                    enabled,
                    ..Default::default()
                },
                crate::infrastructure::host::call_options(10),
            )
            .await?;
        }
        Command::Models(id) => {
            return Ok(ActionResult::Models(
                id.clone(),
                http.list_models_with_options(
                    ene_rpc::ModelsRequest {
                        id: (&connection(&snapshot, &id)?.id).into(),
                        companion_id: Some(c.id.clone()),
                        ..Default::default()
                    },
                    crate::infrastructure::host::call_options(35),
                )
                .await?
                .into_owned()
                .items,
            ));
        }
        Command::Availability(id) => {
            return Ok(ActionResult::Availability(
                http.get_availability_with_options(
                    ene_rpc::AvailabilityRequest {
                        connection_id: id.clone(),
                        companion_id: companion.clone(),
                        ..Default::default()
                    },
                    crate::infrastructure::host::call_options(5),
                )
                .await?
                .into_owned(),
            ));
        }
        Command::Companion {
            revision,
            name,
            mind,
            body,
            voice,
        } => {
            http.update_companion_with_options(
                UpdateCompanionRequest {
                    id: c.id.clone(),
                    revision,
                    name: Some(name),
                    mind: Some(mind),
                    body: body.into(),
                    voice: voice.into(),
                    ..Default::default()
                },
                crate::infrastructure::host::call_options(10),
            )
            .await?;
        }
        Command::RevokePermissions(id) => {
            connection(&snapshot, &id)?;
            for grant in http
                .list_grants_with_options(
                    ene_rpc::GrantsRequest {
                        companion_id: Some(c.id.clone()),
                        ..Default::default()
                    },
                    crate::infrastructure::host::call_options(5),
                )
                .await?
                .into_owned()
                .items
                .into_iter()
                .filter(|g| g.connection_id == id && g.status == "active")
            {
                http.revoke_grant_with_options(
                    ene_rpc::Id {
                        id: grant.id.clone(),
                        ..Default::default()
                    },
                    crate::infrastructure::host::call_options(10),
                )
                .await
                .map(|_| ())?;
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
                let availability = http
                    .get_availability_with_options(
                        ene_rpc::AvailabilityRequest {
                            connection_id: p.id.clone(),
                            companion_id: c.id.clone(),
                            ..Default::default()
                        },
                        crate::infrastructure::host::call_options(5),
                    )
                    .await?
                    .into_owned();
                let required = availability
                    .operations
                    .iter()
                    .filter(|operation| operation.purpose == purpose);
                let fully_granted = required
                    .clone()
                    .all(|operation| operation.authorization == "granted");
                if enabled && !fully_granted {
                    http.create_grant_with_options(
                        CreateGrantRequest {
                            request_key: format!("{key}-{purpose}"),
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
                            consent: consent.clone().into(),
                            ..Default::default()
                        },
                        crate::infrastructure::host::call_options(10),
                    )
                    .await?;
                }
                if !enabled {
                    for g in grants {
                        http.revoke_grant_with_options(
                            ene_rpc::Id {
                                id: g.id.clone(),
                                ..Default::default()
                            },
                            crate::infrastructure::host::call_options(10),
                        )
                        .await
                        .map(|_| ())?;
                    }
                }
            }
        }
        Command::History => unreachable!("history is owned by the synced session"),
        command => match command {
            Command::Credential(id, secret) => {
                let pid = &connection(&snapshot, &id)?.id;
                let result = http
                    .register_credentials_with_options(
                        CredentialRequest {
                            id: pid.into(),
                            secret: Some(secret),
                            source: None,
                            ..Default::default()
                        },
                        crate::infrastructure::host::call_options(10),
                    )
                    .await?
                    .into_owned();
                feedback = if result.credential_state == "stored" {
                    "APIキーを資格情報ストアに保存しました"
                } else {
                    "APIキーは今回のホスト起動中だけ保持します"
                };
            }
            Command::EnvironmentCredential(id) => {
                let pid = &connection(&snapshot, &id)?.id;
                http.register_credentials_with_options(
                    CredentialRequest {
                        id: pid.into(),
                        secret: None,
                        source: Some("environment".into()),
                        ..Default::default()
                    },
                    crate::infrastructure::host::call_options(10),
                )
                .await?;
                feedback = "ホストの環境変数からAPIキーを登録しました";
            }
            Command::RevokeCredential(id) => {
                let pid = &connection(&snapshot, &id)?.id;
                http.revoke_credentials_with_options(
                    ene_rpc::Id {
                        id: pid.into(),
                        ..Default::default()
                    },
                    crate::infrastructure::host::call_options(10),
                )
                .await
                .map(|_| ())?;
                feedback = "資格情報を解除し、生成を停止しました";
            }
            Command::Check(id) => {
                let pid = &connection(&snapshot, &id)?.id;
                confirm_check(
                    &http,
                    &snapshot.host_id,
                    CheckRequest {
                        id: pid.clone(),
                        request_key: key,
                        companion_id: c.id.clone(),
                        purpose: "companion".into(),
                        model_id: c
                            .model_assignments
                            .companion
                            .as_option()
                            .filter(|a| a.connection_id == *pid)
                            .map(|a| a.model_id.clone()),
                        ..Default::default()
                    },
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

/// Confirmation reuses the exact receipt identity and never replays on a new host.
async fn confirm_check(
    http: &Http,
    host: &str,
    input: CheckRequest,
) -> Result<CheckResult, ClientError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let result = tokio::time::timeout_at(
            deadline,
            http.check_connection_with_options(
                input.clone(),
                crate::infrastructure::host::call_options(95),
            ),
        )
        .await
        .map_err(|_| ClientError::unavailable("request transport failed"))?
        .map(|response| response.into_owned());
        match result {
            Err(error)
                if matches!(
                    error.code,
                    connectrpc::ErrorCode::Unavailable | connectrpc::ErrorCode::DeadlineExceeded
                ) => {}
            Err(error)
                if super::transport::api_failure(&error)
                    .is_some_and(|failure| failure.error.code == "request_in_progress") => {}
            result => return result,
        }
        let snapshot = tokio::time::timeout_at(
            deadline,
            http.get_snapshot_with_options(
                ene_rpc::SnapshotRequest {
                    subscription_id: None,
                    ..Default::default()
                },
                crate::infrastructure::host::call_options(5),
            ),
        )
        .await
        .map_err(|_| ClientError::unavailable("request transport failed"))??
        .into_owned();
        if snapshot.host_id != host {
            return Err(ClientError::failed_precondition(
                "subscription must be resynchronized",
            ));
        }
        tokio::time::timeout_at(deadline, tokio::time::sleep(Duration::from_millis(200)))
            .await
            .map_err(|_| ClientError::unavailable("request transport failed"))?;
    }
}

#[cfg(test)]
pub(super) async fn action(
    http: Http,
    snapshot: Snapshot,
    companion: String,
    command: Command,
) -> Result<ActionResult, Error> {
    action_with_key(
        http,
        snapshot,
        companion,
        command,
        Uuid::new_v4().to_string(),
    )
    .await
}
#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::infrastructure::host::transport::RuntimeConnection;
    use ene_rpc::UpdateSettingsRequest;
    #[tokio::test]
    async fn configure_updates_selected_connection_and_only_conversation_assignment() {
        let value = serde_json::json!({
            "host_id":"h","environment_id":"e","capabilities":{"json":"{}"},"cursor":{"epoch":"e","seq":0},
            "companions":[{"id":"c","name":"ene","mind":"","body":null,"voice":null,
                "model_assignments":{"task":{"connection_id":"first","model_id":"task-model"}},"availability":{"json":"{}"},"revision":1}],
            "messages":[],"message_cursors":{},"grants":[],
            "settings":{"revision":1,"connections":[
                {"id":"first","kind":"llm","adapter":"dynamic","endpoint":"https://first.test/v1","options":{"json":"{}"},"credential_state":"stored","capabilities":null,"last_check":null},
                {"id":"selected","kind":"llm","adapter":"dynamic","endpoint":"https://selected.test/v1","options":{"json":"{}"},"credential_state":"missing","capabilities":null,"last_check":null}]}
        });
        let snapshot: Snapshot = serde_json::from_value(value).unwrap();
        let mut reply = snapshot.settings.as_option().unwrap().clone();
        reply.revision += 1;
        reply.connections[1].endpoint = "https://updated.test/v1".into();
        let companion = snapshot.companions[0].clone();
        let patched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = patched.clone();
        let app = axum::Router::new()
            .route("/rpc/ene.host.v1.HostService/ListProviders", axum::routing::post(|| async { axum::Json(serde_json::json!({"items":[{
                "id":"dynamic","name":"Dynamic","kind":"plugin","endpoint":"https://selected.test/v1","options":{"json":"{}"},"settings":[],
                "requires_credentials":false,"environment_variable":null,"destinations":["https://selected.test"],"state":"ready","error":null,"revision":"v1","capabilities":{"streaming_text":true,"model_list":true}}]})) }))
            .route("/rpc/ene.host.v1.HostService/UpdateSettings", axum::routing::post(move |axum::Json(input):axum::Json<UpdateSettingsRequest>| async move {
                assert_eq!(input.connections.items[0].endpoint,"https://first.test/v1");
                assert_eq!(input.connections.items[1].id.as_deref(),Some("selected"));
                assert_eq!(input.connections.items[1].endpoint,"https://updated.test/v1");
                axum::Json(reply)
            }))
            .route("/rpc/ene.host.v1.HostService/UpdateCompanion", axum::routing::post(move |axum::Json(input):axum::Json<UpdateCompanionRequest>| {
                let observed = observed.clone(); let mut companion = companion.clone();
                async move {
                    assert!(input.task_assignment.is_unset());
                    let Some(ene_rpc::assignment_patch::Change::Set(assignment)) = input.companion_assignment.into_option().unwrap().change else { panic!("set conversation assignment"); };
                    assert_eq!(assignment.connection_id,"selected");
                    companion.model_assignments.get_or_insert_default().companion = (*assignment).into();
                    assert_eq!(companion.model_assignments.task.as_option().unwrap().model_id,"task-model");
                    observed.store(true,std::sync::atomic::Ordering::SeqCst);
                    axum::Json(companion)
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = crate::infrastructure::host::connect(&RuntimeConnection {
            endpoint: format!("http://{}/rpc", listener.local_addr().unwrap()),
            token: "fixture".into(),
        })
        .unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, super::super::connect_fixture::router(app))
                .await
                .unwrap()
        });
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
                options: ene_rpc::JsonDocument {
                    json: "{}".into(),
                    ..Default::default()
                },
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
                "host_id":"h","environment_id":"e","capabilities":{"json":"{}"},"cursor":{"epoch":"e","seq":0},
                "companions":[{"id":"c","name":"ene","mind":"","body":null,"voice":null,"model_assignments":{},"availability":{"json":"{}"},"revision":1}],
                "messages":[],"message_cursors":{},"grants":[],
                "settings":{"revision":1,"connections":[]}
            })).unwrap();
            let stored = Arc::new(Mutex::new(snapshot.settings.as_option().unwrap().clone()));
            let saved = stored.clone();
            let calls = Arc::new(Mutex::new(0));
            let observed = calls.clone();
            let app=axum::Router::new()
                .route("/rpc/ene.host.v1.HostService/ListProviders", axum::routing::post(||async {axum::Json(serde_json::json!({"items":[{
                    "id":"dynamic","name":"Dynamic","kind":"plugin","endpoint":"https://example.test/v1","options":{"json":"{}"},"settings":[],
                    "requires_credentials":true,"environment_variable":null,"destinations":["https://example.test"],"state":"ready","error":null,"revision":"v1","capabilities":{"streaming_text":true,"model_list":true}
                }]}))}))
                .route("/rpc/ene.host.v1.HostService/UpdateSettings", axum::routing::post(move |axum::Json(input):axum::Json<UpdateSettingsRequest>| {
                    let saved=saved.clone();let observed=observed.clone();
                    async move {
                        let mut calls=observed.lock().unwrap();*calls+=1;
                        let mut db=saved.lock().unwrap();
                        if input.revision==db.revision {
                            db.revision+=1;
                            db.connections=input.connections.into_option().unwrap().items.into_iter().map(|c|Connection {
                                id:c.id.expect("caller retains creation identity"),kind:c.kind,adapter:c.adapter,
                                endpoint:c.endpoint,options:c.options,credential_state:"missing".into(),..Default::default() }).collect();
                        } else {
                            assert_eq!(input.connections.items[0].id.as_deref(),Some(db.connections[0].id.as_str()));
                        }
                        if lose_response && *calls==1 {
                            (axum::http::StatusCode::SERVICE_UNAVAILABLE,axum::Json(serde_json::json!({"error":{"code":"unavailable","message":"response lost","details":{}}})))
                        } else {
                            (axum::http::StatusCode::OK,axum::Json(serde_json::to_value(&*db).unwrap()))
                        }
                    }
                }))
                .route("/rpc/ene.host.v1.HostService/RegisterCredentials", axum::routing::post(move ||async move {
                    if lose_response {
                        (axum::http::StatusCode::OK,axum::Json(serde_json::json!({"credential_state":"stored"})))
                    } else {
                        (axum::http::StatusCode::BAD_REQUEST,axum::Json(serde_json::json!({"error":{"code":"invalid_request","message":"invalid key","details":{}}})))
                    }
                }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let http = crate::infrastructure::host::connect(&RuntimeConnection {
                endpoint: format!("http://{}/rpc", listener.local_addr().unwrap()),
                token: "fixture".into(),
            })
            .unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, super::super::connect_fixture::router(app))
                    .await
                    .unwrap()
            });
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
                        options: ene_rpc::JsonDocument {
                            json: "{}".into(),
                            ..Default::default()
                        },
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
                    snapshot.settings = stored.lock().unwrap().clone().into();
                }
            }
            assert_eq!(stored.lock().unwrap().connections.len(), 1);
            assert_eq!(stored.lock().unwrap().connections[0].id, creation);
            server.abort();
        }
    }
}
