//! Settings, policy and approval services.
use super::Services;
use crate::host::{self, Runtime, error::HostError, model, permission, settings, wire};
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use ene_rpc as w;

#[allow(refining_impl_trait)]
impl w::SettingsService for Services {
    async fn get_settings(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Settings> {
        self.read(|app| settings::settings(app.world().resource::<host::db::Store>()))
            .await
    }

    async fn update_settings(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdateSettingsRequest>,
    ) -> ServiceResult<w::Settings> {
        let request = request.to_owned_message();
        let (updated, forgotten) = self
            .runtime
            .call(move |app| settings::update_settings(app.world_mut(), request))
            .await
            .map_err(ConnectError::from)?;
        for reference in forgotten {
            let _ = self.runtime.credentials().forget(&reference).await;
        }
        Response::ok(updated)
    }

    async fn list_pending_approvals(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PendingApprovals> {
        self.read(|app| permission::pending_approvals(app.world().resource::<host::db::Store>()))
            .await
    }

    async fn submit_approval(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SubmitApprovalRequest>,
    ) -> ServiceResult<w::SubmitApprovalResponse> {
        let request = request.to_owned_message();
        self.call(move |app| permission::submit_approval(app.world_mut(), request))
            .await
    }

    async fn list_policy_rules(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PolicyRules> {
        self.read(|app| permission::policy_rules(app.world().resource::<host::db::Store>()))
            .await
    }

    async fn update_policy_rules(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::UpdatePolicyRulesRequest>,
    ) -> ServiceResult<w::PolicyRules> {
        let request = request.to_owned_message();
        self.call(move |app| permission::update_policy_rules(app.world_mut(), request))
            .await
    }

    async fn list_providers(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Providers> {
        Response::ok(w::Providers {
            items: model::descriptors(),
            ..Default::default()
        })
    }

    async fn list_models(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ModelsRequest>,
    ) -> ServiceResult<w::Models> {
        let id = request.id.to_owned();
        let connection = connection(&self.runtime, &id).await?;
        let secret = model::secret(&self.runtime.credentials(), &connection)
            .ok_or_else(|| ConnectError::permission_denied("credential_missing"))?;
        let (items, _) = model::probe(&connection, &secret)
            .await
            .map_err(ConnectError::from)?;
        Response::ok(w::Models {
            items,
            ..Default::default()
        })
    }

    async fn register_credentials(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CredentialRequest>,
    ) -> ServiceResult<w::CredentialResult> {
        let id = request.id.to_owned();
        let secret = request.secret.filter(|secret| !secret.is_empty());
        connection(&self.runtime, &id).await?;
        let secret = secret.ok_or_else(|| ConnectError::invalid_argument("secret is required"))?;
        let reference = format!("connection:{id}");
        self.runtime
            .credentials()
            .save(&reference, secret)
            .await
            .map_err(ConnectError::from)?;
        let saved = self
            .runtime
            .call({
                let id = id.clone();
                let reference = reference.clone();
                move |app| {
                    let store = app.world().resource::<host::db::Store>();
                    let mut connection = store
                        .connection(&id)?
                        .ok_or_else(|| HostError::not_found("connection not found"))?;
                    connection.credential_ref = Some(reference);
                    connection.credential_state = host::settings::CredentialState::Registered;
                    store.save_connection(&connection)
                }
            })
            .await;
        if saved.is_err() {
            let _ = self
                .runtime
                .credentials()
                .forget(&format!("connection:{id}"))
                .await;
        }
        saved.map_err(ConnectError::from)?;
        self.runtime
            .call({
                let id = id.clone();
                move |app| host::settings::credential_changed(app.world_mut(), &id)
            })
            .await
            .map_err(ConnectError::from)?;
        Response::ok(w::CredentialResult {
            credential_state: "registered".into(),
            ..Default::default()
        })
    }

    async fn revoke_credentials(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Empty> {
        let id = request.id.to_owned();
        let reference = format!("connection:{id}");
        self.runtime
            .credentials()
            .invalidate(&reference)
            .map_err(ConnectError::from)?;
        let save_id = id.clone();
        self.runtime
            .call(move |app| {
                let store = app.world().resource::<host::db::Store>();
                let mut connection = store
                    .connection(&save_id)?
                    .ok_or_else(|| HostError::not_found("connection not found"))?;
                connection.credential_ref = None;
                connection.credential_state = host::settings::CredentialState::Missing;
                store.save_connection(&connection)
            })
            .await
            .map_err(ConnectError::from)?;
        self.runtime
            .call({
                let id = id.clone();
                move |app| host::settings::credential_changed(app.world_mut(), &id)
            })
            .await
            .map_err(ConnectError::from)?;
        self.runtime
            .credentials()
            .cleanup(&reference)
            .await
            .map_err(ConnectError::from)?;
        Response::ok(w::Empty::default())
    }

    async fn check_connection(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::CheckRequest>,
    ) -> ServiceResult<w::CheckResult> {
        let id = request.id.to_owned();
        let request_key = request.request_key.to_owned();
        let connection = connection(&self.runtime, &id).await?;
        let secret = model::secret(&self.runtime.credentials(), &connection)
            .ok_or_else(|| ConnectError::permission_denied("credential_missing"))?;
        let (models, capabilities) = model::probe(&connection, &secret)
            .await
            .map_err(ConnectError::from)?;
        let model_id = models
            .first()
            .map_or_else(String::new, |model| model.id.clone());
        let capabilities_text = wire::json(&capabilities);
        self.runtime
            .call({
                let id = id.clone();
                let capabilities = capabilities.clone();
                move |app| {
                    let store = app.world().resource::<host::db::Store>();
                    let mut connection = store
                        .connection(&id)?
                        .ok_or_else(|| HostError::not_found("connection not found"))?;
                    connection.last_check = capabilities;
                    store.save_connection(&connection)
                }
            })
            .await
            .map_err(ConnectError::from)?;
        Response::ok(w::CheckResult {
            request_id: request_key,
            capabilities: capabilities_text,
            success: true,
            model_id,
            ..Default::default()
        })
    }
}

async fn connection(
    runtime: &Runtime,
    id: &str,
) -> Result<host::settings::Connection, ConnectError> {
    let id = id.to_owned();
    runtime
        .call(move |app| {
            app.world()
                .resource::<host::db::Store>()
                .connection(&id)?
                .ok_or_else(|| HostError::not_found("connection not found"))
        })
        .await
        .map_err(ConnectError::from)
}
