//! Settings service.

use super::super::Services;
use crate::host::error::HostError;
use connectrpc::RequestContext;
use connectrpc::ServiceRequest;
use connectrpc::ServiceResult;
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
                let Some(loaded) = providers.get(adapter) else {
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
    services
        .change(move |services| {
            let store = &services.store;
            let (entry, manifest) = crate::host::providers::install_local_package(
                store,
                &services.state_dir,
                std::path::Path::new(&source_path),
            )?;
            let extensions = manifest
                .entrypoints
                .iter()
                .flat_map(|ep| ep.provides.iter())
                .map(|ext| format!("{ext:?}").to_ascii_lowercase())
                .collect();
            Ok(w::PluginInfo {
                id: entry.plugin_id,
                name: entry.name,
                version: entry.version,
                description: manifest.description,
                enabled: entry.enabled,
                extensions,
                path: entry.path,
                installed_at: entry.installed_at,
                ..Default::default()
            })
        })
        .await
}

pub(super) async fn set_plugin_enabled(
    services: &Services,
    _arg1: RequestContext,
    request: ServiceRequest<'_, w::SetPluginEnabledRequest>,
) -> ServiceResult<w::PluginInfo> {
    let req = request.to_owned_message();
    services
        .change(move |services| {
            let store = &services.store;
            if !store
                .plugin_repo()
                .set_plugin_enabled(&req.plugin_id, req.enabled)?
            {
                return Err(HostError::NotFound(format!(
                    "plugin `{}` not found",
                    req.plugin_id
                )));
            }
            let entry = store
                .plugin_repo()
                .plugin_entries()?
                .into_iter()
                .find(|e| e.plugin_id == req.plugin_id)
                .ok_or_else(|| HostError::NotFound("plugin not found".into()))?;
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
    services
        .change(move |services| {
            let store = &services.store;
            if !crate::host::providers::remove_local_package(
                store,
                &services.state_dir,
                &plugin_id,
            )? {
                return Err(HostError::NotFound(format!(
                    "plugin `{plugin_id}` not found"
                )));
            }
            Ok(w::Empty::default())
        })
        .await
}
