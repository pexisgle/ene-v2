use super::*;
pub(super) enum ActionResult {
    Done(String, bool),
    History(Page<Message>),
    Models(Vec<ene_protocol::provider::Model>),
}
fn connection(snapshot: &Snapshot) -> Result<&Value, Error> {
    snapshot
        .settings
        .connections
        .iter()
        .find(|p| {
            snapshot
                .companions
                .first()
                .and_then(|c| c.model_assignments.companion.as_ref())
                .is_some_and(|a| a["connection_id"] == p["id"])
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
            let descriptors: Vec<ene_protocol::provider::ProviderInfo> =
                serde_json::from_value(http.request(Method::GET, "/providers", None, None).await?)
                    .map_err(|_| Error::Transport)?;
            let descriptor = descriptors
                .iter()
                .find(|p| p.id == provider && p.state == "ready")
                .ok_or_else(|| Error::Api("プロバイダーを有効化してください".into()))?;
            let mut merged = descriptor.options.as_object().cloned().unwrap_or_default();
            merged.extend(options.as_object().cloned().ok_or(Error::Transport)?);
            let options = Value::Object(merged);
            let p = snapshot
                .settings
                .connections
                .iter()
                .find(|p| p["adapter"] == provider);
            let mut connections = snapshot.settings.connections.clone();
            if p.is_none() {
                connections.push(
                    json!({"kind":"llm","adapter":provider,"endpoint":endpoint,"options":options}),
                );
            } else if let Some(p) = connections.iter_mut().find(|p| p["adapter"] == provider) {
                p["endpoint"] = json!(endpoint);
                p["options"] = options;
            }
            // Strip server-managed fields when replacing the list.
            let connections:Vec<_>=connections.into_iter().map(|p|json!({"id":p["id"],"kind":p["kind"],"adapter":p["adapter"],"endpoint":p["endpoint"],"options":p["options"]})).collect();
            let saved = http
                .request(
                    Method::PATCH,
                    "/settings",
                    Some(json!({"revision":snapshot.settings.revision,"connections":connections})),
                    None,
                )
                .await?;
            let p = saved["connections"]
                .as_array()
                .and_then(|a| a.iter().find(|p| p["adapter"] == provider))
                .ok_or(Error::Transport)?;
            http.request(Method::PATCH,&format!("/companions/{}",c.id),Some(json!({"revision":c.revision,"model_assignments":{"companion":{"connection_id":p["id"],"model_id":model},"task":{"connection_id":p["id"],"model_id":model}}})),None).await?;
        }
        Command::ReloadProviders => {
            http.request(Method::POST, "/providers/reload", None, None)
                .await?;
        }
        Command::EnableProvider { provider, enabled } => {
            http.request(
                Method::PUT,
                &format!("/providers/{provider}/enabled"),
                Some(json!({"enabled":enabled})),
                None,
            )
            .await?;
        }
        Command::Models => {
            let pid = connection(&snapshot)?["id"]
                .as_str()
                .ok_or(Error::Transport)?;
            return Ok(ActionResult::Models(
                serde_json::from_value(
                    http.request(
                        Method::GET,
                        &format!("/connections/{pid}/models"),
                        None,
                        None,
                    )
                    .await?,
                )
                .map_err(|_| Error::Transport)?,
            ));
        }
        Command::Companion { name, mind } => {
            http.request(
                Method::PATCH,
                &format!("/companions/{}", c.id),
                Some(json!({"revision":c.revision,"name":name,"mind":mind})),
                None,
            )
            .await?;
        }
        Command::Permission {
            conversation,
            check,
        } => {
            let p = connection(&snapshot)?;
            let pid = p["id"].as_str().ok_or(Error::Transport)?;
            let providers: Vec<ene_protocol::provider::ProviderInfo> =
                serde_json::from_value(http.request(Method::GET, "/providers", None, None).await?)
                    .map_err(|_| Error::Transport)?;
            let destinations = &providers
                .iter()
                .find(|d| d.id == p["adapter"])
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
                        g["companion_id"] == c.id
                            && g["connection_id"] == pid
                            && g["purpose"] == purpose
                            && g["status"] == "active"
                    })
                    .collect();
                if enabled && grants.is_empty() {
                    http.request(Method::POST,"/grants",Some(json!({"companion_id":c.id,"kind":"data_transfer","connection_id":pid,"destinations":destinations,"data_kinds":if purpose=="connection_check"{vec![data,"metadata"]}else{vec![data]},"purpose":purpose})),Some(&format!("{key}-{purpose}"))).await?;
                }
                if !enabled {
                    for g in grants {
                        http.request(
                            Method::DELETE,
                            &format!("/grants/{}", g["id"].as_str().ok_or(Error::Transport)?),
                            None,
                            None,
                        )
                        .await?;
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
            let mut url = url::Url::parse(&format!(
                "{}{}",
                http.connection.endpoint,
                format_args!("/companions/{}/messages", c.id)
            ))
            .map_err(|_| Error::Transport)?;
            url.query_pairs_mut().append_pair("cursor", cursor);
            let path = url
                .as_str()
                .strip_prefix(&http.connection.endpoint)
                .ok_or(Error::Transport)?;
            return Ok(ActionResult::History(
                serde_json::from_value(http.request(Method::GET, path, None, None).await?)
                    .map_err(|_| Error::Transport)?,
            ));
        }
        command => {
            let pid = connection(&snapshot)?["id"]
                .as_str()
                .ok_or(Error::Transport)?;
            match command {
                Command::Credential(secret) => {
                    let result = http
                        .request(
                            Method::PUT,
                            &format!("/credentials/{pid}"),
                            Some(json!({"secret":secret})),
                            None,
                        )
                        .await?;
                    feedback = if result["credential_state"] == "stored" {
                        "APIキーを資格情報ストアに保存しました"
                    } else {
                        "APIキーは今回のホスト起動中だけ保持します"
                    };
                }
                Command::EnvironmentCredential => {
                    http.request(
                        Method::PUT,
                        &format!("/credentials/{pid}"),
                        Some(json!({"source":"environment"})),
                        None,
                    )
                    .await?;
                    feedback = "ホストの環境変数からAPIキーを登録しました";
                }
                Command::RevokeCredential => {
                    http.request(Method::DELETE, &format!("/credentials/{pid}"), None, None)
                        .await?;
                    feedback = "資格情報を解除し、生成を停止しました";
                }
                Command::Check => {
                    http.request(
                        Method::POST,
                        &format!("/connections/{pid}/check"),
                        Some(json!({"companion_id":c.id,"purpose":"companion","model_id":c.model_assignments.companion.as_ref().and_then(|a|a.get("model_id"))})),
                        Some(&key),
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
