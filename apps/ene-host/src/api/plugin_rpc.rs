//! Authenticated Connect plugin management; the runtime never owns host identity.
use super::{
    rpc::{failure, invalid},
    state::App,
};
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use ene_api_types::{plugin as p, rpc as w, rpc_conversion::RpcConvert};
pub(super) struct PluginRpc {
    pub app: App,
}
fn wire<T: RpcConvert>(v: T) -> Result<T::Wire, ConnectError> {
    v.into_wire()
        .map_err(|_| ConnectError::internal("応答を変換できません"))
}
fn read<T: RpcConvert>(v: T::Wire) -> Result<T, ConnectError> {
    T::from_wire(v).map_err(|_| invalid())
}
fn plugins(app: &App) -> Result<w::Plugins, ConnectError> {
    Ok(w::Plugins {
        items: app
            .application
            .plugins()
            .into_iter()
            .map(wire)
            .collect::<Result<_, _>>()?,
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
                failure(ene_core::domain::Error::new(
                    ene_core::domain::ErrorKind::NotFound,
                ))
            })?;
        Response::ok(wire(value)?)
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
            .enable_plugin(
                req.id.to_owned(),
                p::PluginEnabled {
                    enabled: req.enabled,
                    revision: req.revision.to_owned(),
                },
            )
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
                .map_err(failure)?
                .into_iter()
                .map(wire)
                .collect::<Result<_, _>>()?,
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
            .bind_plugin(read(req.to_owned_message())?)
            .await
            .map_err(failure)?;
        Response::ok(w::PluginBindings {
            items: self
                .app
                .application
                .plugin_bindings()
                .await
                .map_err(failure)?
                .into_iter()
                .map(wire)
                .collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }
    async fn invoke(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, w::PluginInvocation>,
    ) -> ServiceResult<w::PluginResult> {
        Response::ok(wire(
            self.app
                .application
                .invoke_plugin(read(req.to_owned_message())?)
                .await
                .map_err(failure)?,
        )?)
    }
}
