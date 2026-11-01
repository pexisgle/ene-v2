//! Authenticated Connect plugin management; the runtime never owns host identity.
use super::{rpc::failure, state::App};
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use ene_rpc as w;
pub(super) struct PluginRpc {
    pub app: App,
}
fn plugins(app: &App) -> Result<w::Plugins, ConnectError> {
    Ok(w::Plugins {
        items: app.application.plugins(),
        ..Default::default()
    })
}
#[allow(refining_impl_trait)]
impl w::PluginService for PluginRpc {
    async fn list(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Plugins> {
        Response::ok(plugins(&self.app)?)
    }
    async fn get(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::Plugin> {
        let value = self
            .app
            .application
            .plugins()
            .into_iter()
            .find(|p| p.id == req.id)
            .ok_or_else(|| {
                failure(crate::domain::Error::new(
                    crate::domain::ErrorKind::NotFound,
                ))
            })?;
        Response::ok(value)
    }
    async fn reload(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Plugins> {
        self.app
            .application
            .reload_plugins()
            .await
            .map_err(failure)?;
        Response::ok(plugins(&self.app)?)
    }
    async fn enable(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::EnablePluginRequest>,
    ) -> ServiceResult<w::Plugins> {
        self.app
            .application
            .enable_plugin(req.id.to_owned(), req.to_owned_message())
            .await
            .map_err(failure)?;
        Response::ok(plugins(&self.app)?)
    }
    async fn list_bindings(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::PluginBindings> {
        Response::ok(w::PluginBindings {
            items: self
                .app
                .application
                .plugin_bindings()
                .await
                .map_err(failure)?,
            ..Default::default()
        })
    }
    async fn bind(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::PluginBinding>,
    ) -> ServiceResult<w::PluginBindings> {
        self.app
            .application
            .bind_plugin(req.to_owned_message())
            .await
            .map_err(failure)?;
        Response::ok(w::PluginBindings {
            items: self
                .app
                .application
                .plugin_bindings()
                .await
                .map_err(failure)?,
            ..Default::default()
        })
    }
    async fn invoke(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::PluginInvocation>,
    ) -> ServiceResult<w::PluginResult> {
        Response::ok(
            self.app
                .application
                .invoke_plugin(req.to_owned_message())
                .await
                .map_err(failure)?,
        )
    }
    async fn get_harness(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::HarnessState> {
        let binding_id = req.id.to_owned();
        let state = self
            .app
            .application
            .harness_state(binding_id.clone())
            .await
            .map_err(failure)?
            .ok_or_else(|| {
                failure(crate::domain::Error::new(
                    crate::domain::ErrorKind::NotFound,
                ))
            })?;
        Response::ok(w::HarnessState {
            binding_id,
            state: ene_rpc::JsonDocument {
                json: state.to_string(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        })
    }
}
