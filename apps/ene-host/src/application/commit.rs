//! The only application boundary for durable configuration and its notifications.
use super::{HostApplication, generation::*};
use crate::{
    domain::*,
    ports::{ConfigurationCommit, Receipt},
};

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
    let reconfigured = super::conversation::reconfigured(app, controls, &configuration);
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
    // A relevant settings/grant/credential change is a reason to retry index
    // work that had exhausted its bounded attempts.
    let _ = app
        .0
        .repository
        .mind_retry_index(crate::application::mind::now())
        .await;
    app.notify_mind_work();
    // A turn started under the old model, credential or grant stops now; the
    // next input reconnects with the committed configuration.
    if let Ok(runtime) = app.rig() {
        for companion in reconfigured {
            let _ = runtime.stop(companion, Some("設定または権限が変更されました".into()));
        }
    }
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
