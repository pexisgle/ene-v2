//! Local plugin package installation and removal on disk.
use super::plugins_dir;
use crate::host::{
    db::{Result, Store, plugins::PluginEntry},
    error::HostError,
};
use ene_plugin_runtime::Package;
use private_fs::PrivateDir;
use std::{
    fs,
    path::{Path, PathBuf},
};

const MANAGED: &str = ".managed";

/// Copies a verified snapshot to an immutable revision directory, then commits
/// which revision the host may execute. An unrecorded directory is inert.
pub(crate) fn install_local_package(
    store: &Store,
    state_dir: &Path,
    source_dir: &Path,
) -> Result<(PluginEntry, Package)> {
    let source = Package::load(source_dir)
        .map_err(|error| HostError::Invalid(format!("invalid plugin package: {error}")))?;
    let installed = publish_package(&plugins_dir(state_dir), &source)?;
    let manifest = &installed.manifest;
    let entry = store.plugin_repo().install_plugin_entry(
        &manifest.id,
        &manifest.name,
        &manifest.version,
        &installed.root.to_string_lossy(),
        &installed.revision,
    )?;
    Ok((entry, installed))
}

fn publish_package(root: &Path, source: &Package) -> Result<Package> {
    fs::create_dir_all(root).map_err(package_storage)?;
    let managed = root.join(MANAGED);
    let directory = PrivateDir::ensure(&managed)
        .and_then(|root| root.ensure_dir(&source.manifest.id))
        .map_err(package_storage)?;
    let revisions = managed.join(&source.manifest.id);
    let destination = revisions.join(&source.revision);
    if !destination.try_exists().map_err(package_storage)? {
        let stage = tempfile::tempdir_in(&revisions).map_err(package_storage)?;
        write_package(stage.path(), source)?;
        fs::rename(stage.path(), &destination).map_err(package_storage)?;
        directory.sync_all().map_err(package_storage)?;
    }
    let installed = Package::load(&destination.canonicalize().map_err(package_storage)?)
        .map_err(package_storage)?;
    if installed.revision != source.revision {
        return Err(HostError::Storage(
            "the installed plugin revision does not match the verified source".into(),
        ));
    }
    Ok(installed)
}

fn write_package(path: &Path, source: &Package) -> Result<()> {
    let dir = PrivateDir::open(path).map_err(package_storage)?;
    let names = std::iter::once("manifest.json").chain(
        source
            .manifest
            .entrypoints
            .iter()
            .map(|entry| entry.file.as_str())
            .filter(|name| !name.is_empty()),
    );
    for name in names.collect::<std::collections::BTreeSet<_>>() {
        let relative = Path::new(name);
        if let Some(parent) = relative
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            dir.ensure_dir_all(parent).map_err(package_storage)?;
        }
        let bytes = source.code(name).map_err(package_storage)?;
        dir.write_atomic_durable(relative, bytes)
            .map_err(package_storage)?;
    }
    Ok(())
}

fn package_storage(error: impl std::fmt::Display) -> HostError {
    HostError::Storage(format!("plugin package storage: {error}"))
}

/// Removes managed revisions. The registry row stays disabled until cleanup succeeds.
pub(crate) fn remove_local_package(
    store: &Store,
    state_dir: &Path,
    plugin_id: &str,
) -> Result<bool> {
    if !ene_plugin_runtime::package::identifier(plugin_id) {
        return Err(HostError::Invalid("invalid plugin id".into()));
    }
    let repo = store.plugin_repo();
    let registered = repo.set_plugin_enabled(plugin_id, false)?.is_some();
    let removed = match fs::remove_dir_all(managed_package_dir(state_dir, plugin_id)) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(package_storage(error)),
    };
    repo.remove_plugin_entry(plugin_id)?;
    Ok(registered || removed)
}

pub(super) fn managed_package_dir(state: &Path, id: &str) -> PathBuf {
    plugins_dir(state).join(MANAGED).join(id)
}

pub(super) fn is_managed_revision(path: &Path, id: &str, revision: &str) -> bool {
    path.ends_with(Path::new(MANAGED).join(id).join(revision))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t18_plugin_install_list_toggle_and_remove_lifecycle() {
        let state = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let store = Store::open(&state.path().join("host.sqlite3")).unwrap();

        write_test_package(src.path()).unwrap();

        let (entry, parsed) = install_local_package(&store, state.path(), src.path()).unwrap();
        assert_eq!(entry.plugin_id, "demo-tool");
        assert_eq!(parsed.manifest.version, "0.2.0");
        assert!(entry.enabled);
        assert_eq!(entry.revision, parsed.revision);
        assert_eq!(store.plugin_repo().plugin_entries().unwrap().len(), 1);
        let (reused, _) = install_local_package(&store, state.path(), &parsed.root).unwrap();
        assert_eq!(reused.path, entry.path);
        assert!(parsed.root.join("main.wasm").is_file());
        let registry = super::super::Providers::load(state.path(), &store).unwrap();
        assert_eq!(
            registry.get("demo-tool").unwrap().package.revision,
            parsed.revision
        );

        assert!(
            store
                .plugin_repo()
                .set_plugin_enabled("demo-tool", false)
                .unwrap()
                .is_some()
        );
        assert!(!store.plugin_repo().plugin_entries().unwrap()[0].enabled);

        assert_failed_removal_preserves_the_disabled_entry(&store, state.path(), &parsed.root)
            .unwrap();
        assert!(remove_local_package(&store, state.path(), "demo-tool").unwrap());
        assert_eq!(
            store.plugin_repo().plugin_entries().unwrap(),
            [] as [ene_store::PluginEntry; 0]
        );
        assert!(!parsed.root.exists());
    }

    fn write_test_package(source: &Path) -> std::io::Result<()> {
        fs::write(source.join("main.wasm"), b"\0asm")?;
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
        fs::write(source.join("manifest.json"), manifest.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(source, fs::Permissions::from_mode(0o700))?;
            for name in ["main.wasm", "manifest.json"] {
                fs::set_permissions(source.join(name), fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(())
    }

    fn assert_failed_removal_preserves_the_disabled_entry(
        store: &Store,
        state: &Path,
        managed: &Path,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let directory = managed_package_dir(state, "demo-tool");
        let held = directory.with_file_name("demo-tool-held");
        fs::rename(&directory, &held)?;
        fs::write(&directory, b"occupied")?;
        assert!(remove_local_package(store, state, "demo-tool").is_err());
        assert!(!store.plugin_repo().plugin_entries()?[0].enabled);
        assert!(
            held.join(managed.file_name().unwrap())
                .join("main.wasm")
                .is_file()
        );
        fs::remove_file(&directory)?;
        fs::rename(held, directory)?;
        Ok(())
    }
}
