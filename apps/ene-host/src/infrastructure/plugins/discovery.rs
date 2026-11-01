//! Package discovery and metadata scanning. No guest code runs here.
use super::{Entry, Manager, failure};
use crate::infrastructure::llm::plugin::manifest;
use crate::ports::Result;
use ene_plugin_runtime::Package;
use std::{collections::BTreeMap, sync::atomic::Ordering};

impl Manager {
    pub(super) fn scan(&self) -> Result<()> {
        let (packages, errors) = self.load_packages()?;
        self.apply_scan(packages, errors)
    }
    pub(super) fn load_packages(&self) -> Result<(BTreeMap<String, Package>, Vec<String>)> {
        let mut packages = BTreeMap::new();
        let mut provider_ids = std::collections::BTreeSet::new();
        let mut errors = Vec::new();
        if self.root.exists() {
            let mut paths = std::fs::read_dir(&self.root)
                .map_err(failure)?
                .map(|entry| entry.map(|entry| entry.path()).map_err(failure))
                .collect::<Result<Vec<_>>>()?;
            paths.sort();
            for path in paths {
                if !std::fs::symlink_metadata(&path).map_err(failure)?.is_dir() {
                    continue;
                }
                let package = match Package::load(&path) {
                    Ok(package) => package,
                    Err(error) => {
                        errors.push(format!(
                            "{}: {error}",
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("package")
                        ));
                        continue;
                    }
                };
                let mut package_providers = Vec::new();
                let mut provider_error = false;
                for entry in &package.manifest.entrypoints {
                    if let Some(provider) = &entry.provider {
                        if manifest::validate(provider).is_err()
                            || provider_ids.contains(&provider.id)
                        {
                            provider_error = true;
                            break;
                        }
                        package_providers.push(provider.id.clone());
                    }
                }
                if provider_error {
                    errors.push(format!("{}: provider", package.manifest.id));
                    continue;
                }
                let id = package.manifest.id.clone();
                if packages.contains_key(&id) {
                    errors.push(format!("{id}: duplicate package id"));
                    continue;
                }
                for provider in package_providers {
                    provider_ids.insert(provider);
                }
                packages.insert(id, package);
            }
        }
        Ok((packages, errors))
    }
    /// Merge a finished scan into the entries as they are now, not as they were when it started.
    pub(super) fn apply_scan(
        &self,
        packages: BTreeMap<String, Package>,
        errors: Vec<String>,
    ) -> Result<()> {
        let mut entries = self.entries.write().map_err(failure)?;
        let mut next = BTreeMap::new();
        for (id, package) in packages {
            let entry = match entries.get(&id) {
                // Keep the management state; only the verified file snapshot is refreshed.
                Some(e)
                    if e.package.root == package.root && e.package.revision == package.revision =>
                {
                    Entry {
                        package,
                        ..e.clone()
                    }
                }
                _ => Entry {
                    package,
                    state: "disabled".into(),
                    error: None,
                    cancellation: self.stopped.child_token(),
                    generation: self.generations.fetch_add(1, Ordering::Relaxed),
                    capabilities: BTreeMap::new(),
                },
            };
            next.insert(id, entry);
        }
        for (id, entry) in entries.iter() {
            if !next
                .get(id)
                .is_some_and(|e| e.generation == entry.generation)
            {
                entry.cancellation.cancel();
            }
        }
        *entries = next;
        drop(entries);
        *self.discovery_error.write().map_err(failure)? = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };
        Ok(())
    }
}
