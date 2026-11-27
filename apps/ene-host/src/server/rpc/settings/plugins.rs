//! Settings service.

use super::super::Services;
use super::notify_settings;
use crate::host::{
    HostServices,
    error::{HostError, Result},
    providers::extensions::discover_plugin_tools,
};
use connectrpc::ServiceRequest;
use connectrpc::ServiceResult;
use connectrpc::{ConnectError, RequestContext, Response};
use ene_rpc as w;

pub(super) async fn list_plugins(
    services: &Services,
    _arg1: RequestContext,
    _arg2: ServiceRequest<'_, w::Empty>,
) -> ServiceResult<w::PluginList> {
    services
        .inspect(|services| {
            let store = &services.store;
            let providers = &services.providers;
            let entries = store.plugin_repo().plugin_entries()?;
            let mut by_id: std::collections::BTreeMap<String, w::PluginInfo> = entries
                .into_iter()
                .map(|entry| {
                    let info = w::PluginInfo {
                        id: entry.plugin_id.clone(),
                        name: entry.name,
                        version: entry.version,
                        enabled: entry.enabled,
                        path: entry.path,
                        installed_at: entry.installed_at,
                        ..Default::default()
                    };
                    (entry.plugin_id, info)
                })
                .collect();
            for adapter in providers.adapters() {
                let Some(loaded) = providers.get(&adapter) else {
                    continue;
                };
                let manifest = &loaded.package.manifest;
                let extensions: Vec<String> = manifest
                    .entrypoints
                    .iter()
                    .flat_map(|ep| ep.provides.iter())
                    .map(|ext| format!("{ext:?}").to_ascii_lowercase())
                    .collect();
                by_id
                    .entry(manifest.id.clone())
                    .and_modify(|info| {
                        info.description = manifest.description.clone();
                        info.extensions = extensions.clone();
                    })
                    .or_insert_with(|| w::PluginInfo {
                        id: manifest.id.clone(),
                        name: manifest.name.clone(),
                        version: manifest.version.clone(),
                        description: manifest.description.clone(),
                        enabled: true,
                        extensions,
                        path: loaded.package.root.to_string_lossy().into_owned(),
                        ..Default::default()
                    });
            }
            Ok(w::PluginList {
                plugins: by_id.into_values().collect(),
                ..Default::default()
            })
        })
        .await
}

pub(super) async fn install_plugin(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::InstallPluginRequest>,
) -> ServiceResult<w::PluginInfo> {
    let source_path = request.to_owned_message().source_path;
    change_plugin(services, true, move |services| {
        let store = &services.store;
        let (entry, package) = crate::host::providers::install_local_package(
            store,
            &services.state_dir,
            std::path::Path::new(&source_path),
        )?;
        let manifest = &package.manifest;
        let extensions = manifest
            .entrypoints
            .iter()
            .flat_map(|ep| ep.provides.iter())
            .map(|ext| format!("{ext:?}").to_ascii_lowercase())
            .collect();
        let view = w::PluginInfo {
            id: entry.plugin_id,
            name: entry.name,
            version: entry.version,
            description: manifest.description.clone(),
            enabled: entry.enabled,
            extensions,
            path: entry.path,
            installed_at: entry.installed_at,
            ..Default::default()
        };
        services.providers.replace(package, entry.installation_id);
        notify_settings(
            &services.notifier,
            w::Settings::from(&store.settings_repo().settings()?),
        );
        Ok(view)
    })
    .await
}

pub(super) async fn set_plugin_enabled(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::SetPluginEnabledRequest>,
) -> ServiceResult<w::PluginInfo> {
    let req = request.to_owned_message();
    let enabled = req.enabled;
    change_plugin(services, enabled, move |services| {
        let store = &services.store;
        let previous = services.providers.revision();
        let entry = store
            .plugin_repo()
            .set_plugin_enabled(&req.plugin_id, enabled)?
            .ok_or_else(|| HostError::NotFound(format!("plugin `{}` not found", req.plugin_id)))?;
        services
            .providers
            .set_enabled(&entry.plugin_id, entry.enabled);
        if services.providers.revision() != previous {
            notify_settings(
                &services.notifier,
                w::Settings::from(&store.settings_repo().settings()?),
            );
        }
        Ok(w::PluginInfo {
            id: entry.plugin_id,
            name: entry.name,
            version: entry.version,
            enabled: entry.enabled,
            path: entry.path,
            installed_at: entry.installed_at,
            ..Default::default()
        })
    })
    .await
}

pub(super) async fn remove_plugin(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::RemovePluginRequest>,
) -> ServiceResult<w::Empty> {
    let plugin_id = request.to_owned_message().plugin_id;
    change_plugin(services, false, move |services| {
        let store = &services.store;
        let revision = store.settings_repo().settings()?;
        let affected: Vec<_> = revision
            .connections
            .into_iter()
            .filter_map(|mut connection| {
                let extension = crate::host::providers::connection_extension(connection.kind)?;
                (connection.enabled
                    && connection.endpoint().adapter == plugin_id
                    && services.providers.claims(&plugin_id, extension))
                .then(|| {
                    connection.enabled = false;
                    let expected = connection.revision;
                    (connection, expected)
                })
            })
            .collect();
        if !affected.is_empty() {
            let settings = store
                .settings_repo()
                .update_settings(revision.revision, &affected)?;
            notify_settings(&services.notifier, w::Settings::from(&settings));
        }
        services.providers.set_enabled(&plugin_id, false);
        if !crate::host::providers::remove_local_package(store, &services.state_dir, &plugin_id)? {
            return Err(HostError::NotFound(format!(
                "plugin `{plugin_id}` not found"
            )));
        }
        services.providers.remove(&plugin_id);
        Ok(w::Empty::default())
    })
    .await
}

async fn change_plugin<T: Send + 'static>(
    services: &Services,
    prewarm: bool,
    operation: impl FnOnce(&HostServices) -> Result<T> + Send + 'static,
) -> ServiceResult<T> {
    let runtime = services.runtime.clone();
    let executor = tokio::runtime::Handle::current();
    let (value, refresh) = services
        .runtime
        .change(move |shared| {
            let before = shared.providers.revision();
            let value = operation(shared);
            // Accepted management jobs finish publication even if the caller
            // drops the RPC response channel.
            let refresh = (value.is_ok() || shared.providers.revision() != before).then(|| {
                executor.spawn(async move {
                    let shared = runtime.services();
                    if prewarm {
                        shared.providers.start_used(&shared.store).await;
                    }
                    let tools = discover_plugin_tools(&shared.providers, &shared.store).await;
                    runtime
                        .call_kernel(move |kernel| {
                            kernel.register_plugin_tools(tools);
                            Ok(())
                        })
                        .await
                })
            });
            Ok((value?, refresh))
        })
        .await?;
    if let Some(refresh) = refresh {
        refresh
            .await
            .map_err(|_| ConnectError::unavailable("plugin tool publication stopped"))??;
    }
    Response::ok(value)
}
