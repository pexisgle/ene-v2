use super::*;

pub(super) async fn call(
    clients: Clients,
    updates: mpsc::Sender<Update>,
    device: &str,
    active: w::Activation,
    action: Action,
) -> Result<()> {
    let mut settings = false;
    match action {
        Action::History(companion, before) => {
            let history = clients
                .chat
                .get_history(w::HistoryRequest {
                    companion_id: companion.clone(),
                    limit: 100,
                    before,
                    ..Default::default()
                })
                .await?
                .into_owned();
            updates
                .send(Update::History(companion, before, history))
                .await?;
        }
        Action::Send(query) => {
            let key = query.mutation.request_key.clone();
            let error = clients.chat.send(query).await.err().map(|error| {
                error
                    .message
                    .unwrap_or_else(|| "メッセージを送信できませんでした。".into())
            });
            updates.send(Update::SendResult { key, error }).await?;
        }
        Action::Stop(query) => {
            clients.chat.stop(query).await?;
        }
        Action::StopWork(query) => {
            clients.work.stop(query).await?;
        }
        Action::Answer(query) => {
            clients.settings.answer(query).await?;
        }
        Action::Settings => {
            settings = true;
            let audit = clients
                .settings
                .audit_history(w::AuditQuery {
                    limit: 50,
                    ..Default::default()
                })
                .await?
                .into_owned();
            updates.send(Update::Audit(audit.items)).await?;
        }
        Action::SetupProvider(connection, secret) => {
            let saved = if connection.revision == 0 {
                clients
                    .settings
                    .put_connection(w::ConnectionChange {
                        mutation: mutation(0),
                        connection: Some(connection).into(),
                        ..Default::default()
                    })
                    .await?
                    .into_owned()
            } else {
                connection
            };
            if let Some(secret) = secret {
                let credential = clients
                    .settings
                    .set_credential(w::CredentialChange {
                        mutation: mutation(saved.revision),
                        connection_id: saved.id.clone(),
                        secret: Some(secret),
                        ..Default::default()
                    })
                    .await;
                if let Err(error) = credential {
                    updates
                        .send(Update::Settings(
                            clients
                                .settings
                                .get(w::Empty::default())
                                .await?
                                .into_owned(),
                        ))
                        .await?;
                    return Err(error.into());
                }
            }
            updates
                .send(Update::Settings(
                    clients
                        .settings
                        .get(w::Empty::default())
                        .await?
                        .into_owned(),
                ))
                .await?;
            let models = clients
                .settings
                .models(w::Id {
                    id: saved.id,
                    ..Default::default()
                })
                .await;
            match models {
                Ok(models) => {
                    updates
                        .send(Update::Models(models.into_owned().ids))
                        .await?
                }
                Err(err)
                    if err.code == connectrpc::ErrorCode::Unauthenticated
                        || err.code == connectrpc::ErrorCode::ResourceExhausted =>
                {
                    return Err(err.into());
                }
                Err(_) => updates.send(Update::Models(Vec::new())).await?,
            }
        }
        Action::SetupModel(mut companion, model) => {
            clients
                .settings
                .probe(w::ProbeRequest {
                    connection_id: model.connection_id.clone(),
                    model: Some(model.clone()).into(),
                    ..Default::default()
                })
                .await?;
            companion
                .models
                .retain(|selection| selection.role != model.role);
            for role in [
                w::model_selection::Role::Work,
                w::model_selection::Role::Policy,
            ] {
                if companion.models.iter().all(|item| item.role != role) {
                    companion.models.push(w::ModelSelection {
                        role: role.into(),
                        ..model.clone()
                    });
                }
            }
            companion.models.push(model);
            clients
                .state
                .put_companion(w::CompanionChange {
                    mutation: mutation(companion.revision),
                    companion: Some(companion).into(),
                    ..Default::default()
                })
                .await?;
        }
        Action::PutConnection(query) => {
            clients.settings.put_connection(query).await?;
            settings = true;
        }
        Action::DeleteConnection(query) => {
            clients.settings.delete_connection(query).await?;
            settings = true;
        }
        Action::Credential(query) => {
            clients.settings.set_credential(query).await?;
            settings = true;
        }
        Action::Models(id) => {
            let models = clients
                .settings
                .models(w::Id {
                    id,
                    ..Default::default()
                })
                .await?
                .into_owned();
            updates.send(Update::Models(models.ids)).await?;
        }
        Action::McpTools(id) => {
            let response = clients
                .settings
                .mcp_tools(w::Id {
                    id,
                    ..Default::default()
                })
                .await?;
            updates
                .send(Update::McpTools(response.into_owned().ids))
                .await?;
        }
        Action::Probe(query) => {
            clients.settings.probe(query).await?;
            updates
                .send(Update::Notice("接続を確認しました。".into()))
                .await?;
        }
        Action::Policy(query) => {
            clients.settings.put_policy(query).await?;
            settings = true;
        }
        Action::DeleteRule(query) => {
            clients.settings.delete_rule(query).await?;
            settings = true;
        }
        Action::PutPlugin(query) => {
            clients.settings.put_plugin(query).await?;
            settings = true;
        }
        Action::DeletePlugin(query) => {
            clients.settings.delete_plugin(query).await?;
            settings = true;
        }
        Action::PutCharacter(query) => {
            clients.state.put_character(query).await?;
        }
        Action::DeleteCompanion(query) => {
            clients.state.delete_companion(query).await?;
        }
        Action::PutCompanion(query) => {
            clients.state.put_companion(query).await?;
        }
        Action::Preview(query) => {
            let preview = clients.state.preview(query).await?.into_owned();
            updates.send(Update::Notice(preview.text)).await?;
        }
        Action::Import(path) => {
            ensure!(
                tokio::fs::metadata(&path).await?.len() <= 48 * 1024 * 1024,
                "character file too large"
            );
            let package = serde_json::from_slice(&tokio::fs::read(path).await?)?;
            clients
                .state
                .import_character(w::ImportRequest {
                    mutation: mutation(0),
                    package: Some(package).into(),
                    ..Default::default()
                })
                .await?;
        }
        Action::Export(request, path) => {
            let package = clients.state.export_character(request).await?.into_owned();
            save_new(path, &package).await?;
            updates
                .send(Update::Notice(if package.warnings.is_empty() {
                    "書き出しました。".into()
                } else {
                    package.warnings.join("\n")
                }))
                .await?;
        }
        Action::Capture(_, pcm, rate, diagnostic) => {
            ensure!(active.device_id == device, "another device is active");
            let pieces = pcm
                .chunks(16384)
                .map(|chunk| w::AudioInput {
                    device_id: device.into(),
                    generation: active.generation,
                    sample_rate: rate,
                    pcm: chunk.to_vec(),
                    diagnostic,
                    ..Default::default()
                })
                .chain(std::iter::once(w::AudioInput {
                    device_id: device.into(),
                    generation: active.generation,
                    sample_rate: rate,
                    commit: true,
                    diagnostic,
                    request_key: id(),
                    ..Default::default()
                }))
                .collect::<Vec<_>>();
            let mut stream = clients.voice.input().await?;
            for piece in pieces {
                stream.send(piece).await?;
            }
            stream.close_send();
            while let Some(response) = stream.message::<w::Transcription>().await? {
                if diagnostic {
                    let text = response.to_owned_message().text;
                    updates.send(Update::Transcribed(text)).await?;
                }
            }
        }
        Action::TestSpeech(companion_id, text) => {
            clients
                .voice
                .speak(w::SpeakRequest {
                    mutation: mutation(0),
                    companion_id,
                    text,
                    ..Default::default()
                })
                .await?;
        }
        Action::Interrupt(_) => {
            if active.device_id == device {
                let mut stream = clients.voice.input().await?;
                stream
                    .send(w::AudioInput {
                        device_id: device.into(),
                        generation: active.generation,
                        interrupt: true,
                        ..Default::default()
                    })
                    .await?;
                stream.close_send();
                while stream.message::<w::Transcription>().await?.is_some() {}
            }
        }
        Action::Activate(_)
        | Action::AvatarScale(_)
        | Action::ResetHistory
        | Action::Workspace(_)
        | Action::Eyes(_)
        | Action::Input(_)
        | Action::Speaker(_)
        | Action::ClientPlugin { .. } => {}
    }
    if settings {
        updates
            .send(Update::Settings(
                clients
                    .settings
                    .get(w::Empty::default())
                    .await?
                    .into_owned(),
            ))
            .await?;
    }
    Ok(())
}

async fn save_new(path: PathBuf, package: &impl Serialize) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let bytes = serde_json::to_vec(package)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(&bytes).await?;
    file.sync_all().await?;
    Ok(())
}
