use super::*;
use ene_api_types::host::*;
pub(super) enum ActionResult {
    Done(String, bool),
    History(Page<Message>),
    Models(Vec<ene_api_types::provider::Model>),
}
fn connection(snapshot: &Snapshot) -> Result<&Connection, Error> {
    snapshot
        .settings
        .connections
        .iter()
        .find(|p| {
            snapshot
                .companions
                .first()
                .and_then(|c| c.model_assignments.companion.as_ref())
                .is_some_and(|a| a.connection_id == p.id)
        })
        .or_else(|| snapshot.settings.connections.first())
        .ok_or_else(|| Error::Api("先に接続とモデルを設定してください".into()))
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
        Command::ReloadProviders => {
            http.reload_providers().await?;
        }
        Command::EnableProvider { provider, enabled } => {
            http.enable_provider(&provider, enabled).await?;
        }
        Command::Models => {
            return Ok(ActionResult::Models(
                http.models(&connection(&snapshot)?.id, None).await?,
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
            conversation,
            check,
        } => {
            let p = connection(&snapshot)?;
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
        command => {
            let pid = &connection(&snapshot)?.id;
            match command {
                Command::Credential(secret) => {
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
                Command::EnvironmentCredential => {
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
                Command::RevokeCredential => {
                    http.revoke_credentials(pid).await?;
                    feedback = "資格情報を解除し、生成を停止しました";
                }
                Command::Check => {
                    http.check(
                        pid,
                        &CheckInput {
                            companion_id: c.id.clone(),
                            purpose: "companion".into(),
                            model_id: c
                                .model_assignments
                                .companion
                                .as_ref()
                                .map(|a| a.model_id.clone()),
                        },
                        &IdempotencyKey::new(key).map_err(Error::from)?,
                    )
                    .await?;
                    feedback = "実モデルの逐次応答を確認しました";
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(ActionResult::Done(feedback.into(), committed))
}
