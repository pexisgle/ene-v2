//! Local plugin package installation and removal on disk.
use super::plugins_dir;
use crate::host::{
    db::{Result, Store, plugins::PluginEntry},
    error::HostError,
};
use ene_plugin_runtime::Package;
use std::path::Path;

/// Validates a local plugin package directory with `ene-plugin-runtime`, copies
/// its verified files into `<plugins_dir>/<id>/`, and records it in `plugin_entries`.
pub(crate) fn install_local_package(
    store: &Store,
    state_dir: &Path,
    source_dir: &Path,
) -> Result<(PluginEntry, Package)> {
    let package = Package::load(source_dir)
        .map_err(|error| HostError::Invalid(format!("invalid plugin package: {error}")))?;
    let manifest = package.manifest.clone();
    let dest = plugins_dir(state_dir).join(&manifest.id);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .map_err(|error| HostError::Storage(format!("replace existing plugin: {error}")))?;
    }
    std::fs::create_dir_all(&dest)
        .map_err(|error| HostError::Storage(format!("create plugin directory: {error}")))?;
    copy_verified_file(
        &source_dir.join("manifest.json"),
        &dest.join("manifest.json"),
    )?;
    for ep in &manifest.entrypoints {
        if !ep.file.is_empty() {
            let src = source_dir.join(&ep.file);
            let dst = dest.join(&ep.file);
            copy_verified_file(&src, &dst)?;
        }
    }
    // Re-verify the installed copy before recording it.
    let installed = Package::load(&dest).map_err(|error| {
        HostError::Storage(format!("installed plugin verification failed: {error}"))
    })?;
    let entry = store.plugin_repo().upsert_plugin_entry(
        &manifest.id,
        &manifest.name,
        &manifest.version,
        &dest.to_string_lossy(),
        true,
    )?;
    Ok((entry, installed))
}

/// Removes an installed plugin directory under `<plugins_dir>/<id>/` and deletes
/// its SQLite entry, config, and state.
pub(crate) fn remove_local_package(
    store: &Store,
    state_dir: &Path,
    plugin_id: &str,
) -> Result<bool> {
    if !ene_plugin_runtime::package::identifier(plugin_id) {
        return Err(HostError::Invalid("invalid plugin id".into()));
    }
    let removed_row = store
        .plugin_repo()
        .remove_plugin_entry(plugin_id)?
        .is_some();
    let dest = plugins_dir(state_dir).join(plugin_id);
    let removed_dir = if dest.is_dir() {
        std::fs::remove_dir_all(&dest)
            .map_err(|error| HostError::Storage(format!("remove plugin directory: {error}")))?;
        true
    } else {
        false
    };
    Ok(removed_row || removed_dir)
}

fn copy_verified_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| HostError::Storage(format!("plugin subdir: {error}")))?;
    }
    std::fs::copy(src, dst)
        .map_err(|error| HostError::Storage(format!("copy plugin file: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(error) = std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(%error, "the plugin file permissions could not be tightened");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t18_plugin_install_list_toggle_and_remove_lifecycle() {
        let state = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let store = Store::open(&state.path().join("host.sqlite3")).unwrap();

        std::fs::write(src.path().join("main.wasm"), b"\0asm").unwrap();
        let manifest = serde_json::json!({
            "contract_version": ene_plugin::CONTRACT_VERSION,
            "id": "demo-tool",
            "name": "Demo Tool",
            "version": "0.2.0",
            "entrypoints": [{
                "id": "main",
                "target": "host",
                "file": "main.wasm",
                "provides": ["tool"],
                "uses": ["log"]
            }]
        });
        std::fs::write(src.path().join("manifest.json"), manifest.to_string()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Err(error) =
                std::fs::set_permissions(src.path(), std::fs::Permissions::from_mode(0o700))
            {
                tracing::warn!(%error, "the plugin file permissions could not be tightened");
            }
            let _tightened = std::fs::set_permissions(
                src.path().join("main.wasm"),
                std::fs::Permissions::from_mode(0o600),
            );
            let _tightened = std::fs::set_permissions(
                src.path().join("manifest.json"),
                std::fs::Permissions::from_mode(0o600),
            );
        }

        let (entry, parsed) = install_local_package(&store, state.path(), src.path()).unwrap();
        assert_eq!(entry.plugin_id, "demo-tool");
        assert_eq!(parsed.manifest.version, "0.2.0");
        assert!(entry.enabled);
        assert_eq!(store.plugin_repo().plugin_entries().unwrap().len(), 1);

        assert!(
            store
                .plugin_repo()
                .set_plugin_enabled("demo-tool", false)
                .unwrap()
                .is_some()
        );
        assert!(!store.plugin_repo().plugin_entries().unwrap()[0].enabled);

        assert!(remove_local_package(&store, state.path(), "demo-tool").unwrap());
        assert_eq!(
            store.plugin_repo().plugin_entries().unwrap(),
            [] as [ene_store::PluginEntry; 0]
        );
        assert!(!plugins_dir(state.path()).join("demo-tool").exists());
    }
}
