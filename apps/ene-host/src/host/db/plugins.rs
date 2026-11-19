//! Per-plugin configuration and isolated key-value state (`F19`).
use super::{Result, Store, now};
use crate::host::error::HostError;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

const MAX_KEY_CHARS: usize = 128;
const MAX_VALUE_BYTES: usize = 64 * 1024;
const MAX_KEYS_PER_PLUGIN: i64 = 256;

impl Store {
    /// Returns the saved JSON configuration for `plugin_id`, defaulting to `{}`.
    pub(crate) fn plugin_config(&self, plugin_id: &str) -> Result<Value> {
        let raw: Option<String> = self
            .conn()?
            .prepare_cached("SELECT config FROM plugin_config WHERE plugin_id = ?1")?
            .query_row([plugin_id], |row| row.get(0))
            .optional()?;
        match raw {
            Some(text) => serde_json::from_str(&text)
                .map_err(|error| HostError::Storage(format!("corrupt plugin_config: {error}"))),
            None => Ok(serde_json::json!({})),
        }
    }

    /// Persists the JSON configuration document for `plugin_id`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set_plugin_config(&self, plugin_id: &str, config: &Value) -> Result<()> {
        let raw = serde_json::to_string(config)
            .map_err(|error| HostError::Invalid(format!("plugin config JSON: {error}")))?;
        if raw.len() > MAX_VALUE_BYTES {
            return Err(HostError::Invalid(
                "plugin configuration exceeds 64 KiB limit".into(),
            ));
        }
        self.conn()?
            .prepare_cached(
                "INSERT INTO plugin_config (plugin_id, config, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(plugin_id) DO UPDATE SET config = excluded.config, \
                 updated_at = excluded.updated_at",
            )?
            .execute(params![plugin_id, raw, now()])?;
        Ok(())
    }

    /// Reads one key from `plugin_id`'s isolated state namespace.
    pub(crate) fn plugin_state_read(&self, plugin_id: &str, key: &str) -> Result<Option<Value>> {
        validate_state_key(key)?;
        let raw: Option<String> = self
            .conn()?
            .prepare_cached("SELECT value FROM plugin_state WHERE plugin_id = ?1 AND key = ?2")?
            .query_row(params![plugin_id, key], |row| row.get(0))
            .optional()?;
        raw.map(|text| {
            serde_json::from_str(&text)
                .map_err(|error| HostError::Storage(format!("corrupt plugin_state: {error}")))
        })
        .transpose()
    }

    /// Writes one key into `plugin_id`'s isolated state namespace.
    pub(crate) fn plugin_state_write(
        &self,
        plugin_id: &str,
        key: &str,
        value: &Value,
    ) -> Result<()> {
        validate_state_key(key)?;
        let raw = serde_json::to_string(value)
            .map_err(|error| HostError::Invalid(format!("plugin state JSON: {error}")))?;
        if raw.len() > MAX_VALUE_BYTES {
            return Err(HostError::Invalid(
                "plugin state value exceeds 64 KiB limit".into(),
            ));
        }
        let conn = self.conn()?;
        let exists: bool = conn
            .prepare_cached("SELECT 1 FROM plugin_state WHERE plugin_id = ?1 AND key = ?2")?
            .query_row(params![plugin_id, key], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            let count: i64 = conn
                .prepare_cached("SELECT COUNT(*) FROM plugin_state WHERE plugin_id = ?1")?
                .query_row([plugin_id], |row| row.get(0))?;
            if count >= MAX_KEYS_PER_PLUGIN {
                return Err(HostError::Invalid("plugin state key limit reached".into()));
            }
        }
        conn.prepare_cached(
            "INSERT INTO plugin_state (plugin_id, key, value, updated_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(plugin_id, key) DO UPDATE SET value = excluded.value, \
             updated_at = excluded.updated_at",
        )?
        .execute(params![plugin_id, key, raw, now()])?;
        Ok(())
    }
}

fn validate_state_key(key: &str) -> Result<()> {
    let trimmed = key.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_KEY_CHARS {
        return Err(HostError::Invalid(
            "plugin state key must be 1-128 characters".into(),
        ));
    }
    Ok(())
}

/// Persisted metadata for an installed plugin package (`F12`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PluginEntry {
    pub(crate) plugin_id: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) path: String,
    pub(crate) enabled: bool,
    pub(crate) installed_at: String,
    pub(crate) updated_at: String,
}

impl Store {
    pub(crate) fn plugin_entries(&self) -> Result<Vec<PluginEntry>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT plugin_id, name, version, path, enabled, installed_at, updated_at \
             FROM plugin_entries ORDER BY plugin_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(PluginEntry {
                    plugin_id: row.get(0)?,
                    name: row.get(1)?,
                    version: row.get(2)?,
                    path: row.get(3)?,
                    enabled: row.get::<_, i64>(4)? != 0,
                    installed_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub(crate) fn upsert_plugin_entry(
        &self,
        plugin_id: &str,
        name: &str,
        version: &str,
        path: &str,
        enabled: bool,
    ) -> Result<PluginEntry> {
        let stamp = now();
        let conn = self.conn()?;
        conn.prepare_cached(
            "INSERT INTO plugin_entries (plugin_id, name, version, path, enabled, installed_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6) \
             ON CONFLICT(plugin_id) DO UPDATE SET name = excluded.name, version = excluded.version, \
             path = excluded.path, enabled = excluded.enabled, updated_at = excluded.updated_at",
        )?
        .execute(params![
            plugin_id,
            name,
            version,
            path,
            if enabled { 1 } else { 0 },
            stamp,
        ])?;
        let entry = conn
            .prepare_cached(
                "SELECT plugin_id, name, version, path, enabled, installed_at, updated_at \
             FROM plugin_entries WHERE plugin_id = ?1",
            )?
            .query_row([plugin_id], |row| {
                Ok(PluginEntry {
                    plugin_id: row.get(0)?,
                    name: row.get(1)?,
                    version: row.get(2)?,
                    path: row.get(3)?,
                    enabled: row.get::<_, i64>(4)? != 0,
                    installed_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?;
        Ok(entry)
    }

    pub(crate) fn set_plugin_enabled(&self, plugin_id: &str, enabled: bool) -> Result<bool> {
        let changed = self
            .conn()?
            .prepare_cached(
                "UPDATE plugin_entries SET enabled = ?2, updated_at = ?3 WHERE plugin_id = ?1",
            )?
            .execute(params![plugin_id, if enabled { 1 } else { 0 }, now()])?;
        Ok(changed > 0)
    }

    pub(crate) fn remove_plugin_entry(&self, plugin_id: &str) -> Result<Option<PluginEntry>> {
        let conn = self.conn()?;
        let existing = conn
            .prepare_cached(
                "SELECT plugin_id, name, version, path, enabled, installed_at, updated_at \
                 FROM plugin_entries WHERE plugin_id = ?1",
            )?
            .query_row([plugin_id], |row| {
                Ok(PluginEntry {
                    plugin_id: row.get(0)?,
                    name: row.get(1)?,
                    version: row.get(2)?,
                    path: row.get(3)?,
                    enabled: row.get::<_, i64>(4)? != 0,
                    installed_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })
            .optional()?;
        if existing.is_some() {
            conn.prepare_cached("DELETE FROM plugin_entries WHERE plugin_id = ?1")?
                .execute([plugin_id])?;
            conn.prepare_cached("DELETE FROM plugin_config WHERE plugin_id = ?1")?
                .execute([plugin_id])?;
            conn.prepare_cached("DELETE FROM plugin_state WHERE plugin_id = ?1")?
                .execute([plugin_id])?;
        }
        Ok(existing)
    }
}

/// Validates a local plugin package directory with `ene-plugin-runtime`, copies
/// its verified files into `<plugins_dir>/<id>/`, and records it in `plugin_entries`.
pub(crate) fn install_local_package(
    store: &Store,
    state_dir: &std::path::Path,
    source_dir: &std::path::Path,
) -> Result<(PluginEntry, ene_plugin::Manifest)> {
    let package = ene_plugin_runtime::Package::load(source_dir)
        .map_err(|error| HostError::Invalid(format!("invalid plugin package: {error}")))?;
    let manifest = package.manifest.clone();
    let dest = crate::host::providers::plugins_dir(state_dir).join(&manifest.id);
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
    let _ = ene_plugin_runtime::Package::load(&dest).map_err(|error| {
        HostError::Storage(format!("installed plugin verification failed: {error}"))
    })?;
    let entry = store.upsert_plugin_entry(
        &manifest.id,
        &manifest.name,
        &manifest.version,
        &dest.to_string_lossy(),
        true,
    )?;
    Ok((entry, manifest))
}

/// Removes an installed plugin directory under `<plugins_dir>/<id>/` and deletes
/// its SQLite entry, config, and state.
pub(crate) fn remove_local_package(
    store: &Store,
    state_dir: &std::path::Path,
    plugin_id: &str,
) -> Result<bool> {
    if !ene_plugin_runtime::package::identifier(plugin_id) {
        return Err(HostError::Invalid("invalid plugin id".into()));
    }
    let removed_row = store.remove_plugin_entry(plugin_id)?.is_some();
    let dest = crate::host::providers::plugins_dir(state_dir).join(plugin_id);
    let removed_dir = if dest.is_dir() {
        std::fs::remove_dir_all(&dest)
            .map_err(|error| HostError::Storage(format!("remove plugin directory: {error}")))?;
        true
    } else {
        false
    };
    Ok(removed_row || removed_dir)
}

fn copy_verified_file(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| HostError::Storage(format!("plugin subdir: {error}")))?;
    }
    std::fs::copy(src, dst)
        .map_err(|error| HostError::Storage(format!("copy plugin file: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600));
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
            let _ = std::fs::set_permissions(src.path(), std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::set_permissions(
                src.path().join("main.wasm"),
                std::fs::Permissions::from_mode(0o600),
            );
            let _ = std::fs::set_permissions(
                src.path().join("manifest.json"),
                std::fs::Permissions::from_mode(0o600),
            );
        }

        let (entry, parsed) = install_local_package(&store, state.path(), src.path()).unwrap();
        assert_eq!(entry.plugin_id, "demo-tool");
        assert_eq!(parsed.version, "0.2.0");
        assert!(entry.enabled);
        assert_eq!(store.plugin_entries().unwrap().len(), 1);

        assert!(store.set_plugin_enabled("demo-tool", false).unwrap());
        assert!(!store.plugin_entries().unwrap()[0].enabled);

        assert!(remove_local_package(&store, state.path(), "demo-tool").unwrap());
        assert!(store.plugin_entries().unwrap().is_empty());
        assert!(
            !crate::host::providers::plugins_dir(state.path())
                .join("demo-tool")
                .exists()
        );
    }
}
