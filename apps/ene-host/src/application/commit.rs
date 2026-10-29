//! The only application boundary for durable configuration and its notifications.
use super::{HostApplication, generation::*};
use crate::{
    domain::*,
    ports::{ConfigurationCommit, Receipt},
};
use serde_json::json;

pub(super) enum ReceiptEffect {
    None,
    CreateGrant(Receipt),
    CompleteCheck(Receipt),
}

pub(super) async fn configuration(
    app: &HostApplication,
    controls: &mut Controls,
    mut configuration: Configuration,
    receipt: ReceiptEffect,
) -> Result<(), Error> {
    // Install irreversible cancellation fences even if the durable change fails.
    cancel_unpermitted(controls, &configuration);
    refresh(app, controls, &mut configuration);
    let change = match receipt {
        ReceiptEffect::None => ConfigurationCommit::Update(configuration.clone()),
        ReceiptEffect::CreateGrant(receipt) => ConfigurationCommit::CreateGrant {
            configuration: configuration.clone(),
            receipt,
        },
        ReceiptEffect::CompleteCheck(receipt) => ConfigurationCommit::CompleteCheck {
            configuration: configuration.clone(),
            receipt,
        },
    };
    let mut boundary = app.0.boundary.lock().await;
    if let Err(error) = app.0.repository.commit_configuration(change).await {
        if error.kind == ErrorKind::StorageFailure {
            controls.fail_storage(app, &mut boundary);
        }
        return Err(error);
    }
    // Snapshot shares this boundary; it cannot observe a committed DB with an old cursor.
    controls.config = Some(configuration.clone());
    for companion in configuration.companions.values() {
        publish(
            &mut boundary,
            app,
            Some(&companion.id),
            ene_rpc::notification::Event::CompanionUpdated(Box::new(companion.clone())),
        );
    }
    publish(
        &mut boundary,
        app,
        None,
        ene_rpc::notification::Event::SettingsUpdated(Box::new(configuration.settings)),
    );
    publish(
        &mut boundary,
        app,
        None,
        ene_rpc::notification::Event::GrantsUpdated(Box::new(ene_rpc::Grants {
            items: configuration.grants.into_iter().map(Into::into).collect(),
            ..Default::default()
        })),
    );
    Ok(())
}

pub(crate) async fn commit(
    app: &HostApplication,
    controls: &mut Controls,
    configuration: Configuration,
) -> Result<(), Error> {
    self::configuration(app, controls, configuration, ReceiptEffect::None).await
}

pub(super) async fn update_message(
    app: &HostApplication,
    id: &str,
    mut update: MessageUpdate,
) -> Result<(), Error> {
    let controls = app.0.controls.lock().await;
    match &update {
        MessageUpdate::Start | MessageUpdate::Append { .. } => check_run(app, &controls, id)?,
        MessageUpdate::Complete if check_run(app, &controls, id).is_err() => {
            update = MessageUpdate::Cancel(Failure {
                code: "cancelled".into(),
                message: "送信許可・資格情報・ホスト終了により生成を停止しました".into(),
                details: ene_rpc::JsonDocument {
                    json: "{}".into(),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            });
        }
        _ => {}
    }
    let mut boundary = app.0.boundary.lock().await;
    let message = app
        .0
        .repository
        .update_message(id.to_owned(), update.clone())
        .await?;
    if let MessageUpdate::Append { offset, text } = update {
        publish(
            &mut boundary,
            app,
            Some(&message.companion_id),
            ene_rpc::notification::Event::MessageDelta(Box::new(ene_rpc::MessageDelta {
                message_id: id.into(),
                offset: offset as u64,
                delta: text,
                ..Default::default()
            })),
        );
    } else {
        publish(
            &mut boundary,
            app,
            Some(&message.companion_id),
            if matches!(update, MessageUpdate::Start) {
                ene_rpc::notification::Event::MessageStarted(Box::new(message.clone()))
            } else {
                ene_rpc::notification::Event::MessageUpdated(Box::new(message.clone()))
            },
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn accept_conversation(
    app: &HostApplication,
    controls: &mut Controls,
    p: Connection,
    context: super::generation::AuthorizedContext,
    id: String,
    key: String,
    body: serde_json::Value,
    prompt: String,
) -> Result<(crate::ports::Outcome, serde_json::Value), Error> {
    let mut boundary = app.0.boundary.lock().await;
    let accepted = app.0.repository.accept(key, id.clone(), body).await?;
    let (receipt, user, response) = match accepted {
        Accepted::Replay { receipt } => {
            return Ok((crate::ports::Outcome::Accepted, json!(receipt)));
        }
        Accepted::New {
            receipt,
            user,
            response,
        } => (receipt, user, response),
    };
    let response_id = receipt.response_message_id.clone();
    // controls is still locked: shutdown cannot close execution registration between SQL and ownership.
    let receiver = register(
        controls,
        response_id.clone(),
        id.clone(),
        &p,
        &context,
        OperationKind::Conversation,
    );
    publish(
        &mut boundary,
        app,
        Some(&id),
        ene_rpc::notification::Event::MessageCreated(user),
    );
    publish(
        &mut boundary,
        app,
        Some(&id),
        ene_rpc::notification::Event::MessageCreated(response),
    );
    let (done, completion) = tokio::sync::watch::channel(None);
    let predecessor = controls.tails.insert(id.clone(), completion);
    app.0.generations.spawn(generate(
        app.clone(),
        id,
        prompt,
        response_id,
        receiver,
        predecessor,
        done,
    ));
    Ok((crate::ports::Outcome::Accepted, json!(receipt)))
}
