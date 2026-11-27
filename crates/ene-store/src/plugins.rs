//! Per-plugin configuration, isolated key-value state, and installed registry entries.

use crate::row::FromRow;
use crate::{Store, StoreError, error::Result, util::now};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

const ENTRY_COLUMNS: &str =
    "plugin_id, name, version, path, enabled, installed_at, updated_at, revision, installation_id";

const MAX_KEY_CHARS: usize = 128;
const MAX_VALUE_BYTES: usize = 64 * 1024;
const MAX_KEYS_PER_PLUGIN: i64 = 256;

/// Persisted metadata for an installed plugin package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginEntry {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub path: String,
    pub enabled: bool,
    pub installed_at: String,
    pub updated_at: String,
    pub revision: Option<String>,
    pub installation_id: String,
}

/// Repository managing `plugin_config`, `plugin_state`, and `plugin_entries`.
#[derive(Clone, Copy, Debug)]
pub struct PluginRepo<'a>(&'a Store);

impl<'a> PluginRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    /// Returns the saved JSON configuration for `plugin_id`, defaulting to `{}`.
    pub fn plugin_config(&self, plugin_id: &str) -> Result<Value> {
        PluginData {
            conn: &self.0.conn(),
            plugin_id,
        }
        .config()
    }

    /// Persists the JSON configuration document for `plugin_id`.
    pub fn set_plugin_config(&self, plugin_id: &str, config: &Value) -> Result<()> {
        let raw = serde_json::to_string(config)
            .map_err(|error| StoreError::Invalid(format!("plugin config JSON: {error}")))?;
        if raw.len() > MAX_VALUE_BYTES {
            return Err(StoreError::Invalid(
                "plugin configuration exceeds 64 KiB limit".into(),
            ));
        }
        self.0
            .conn()
            .prepare_cached(
                "INSERT INTO plugin_config (plugin_id, config, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(plugin_id) DO UPDATE SET config = excluded.config, \
                 updated_at = excluded.updated_at",
            )?
            .execute(params![plugin_id, raw, now()])?;
        Ok(())
    }

    /// Reads one key from `plugin_id`'s isolated state namespace.
    pub fn plugin_state_read(&self, plugin_id: &str, key: &str) -> Result<Option<Value>> {
        PluginData {
            conn: &self.0.conn(),
            plugin_id,
        }
        .read(key)
    }

    /// Writes one key into `plugin_id`'s isolated state namespace.
    pub fn plugin_state_write(&self, plugin_id: &str, key: &str, value: &Value) -> Result<()> {
        PluginData {
            conn: &self.0.conn(),
            plugin_id,
        }
        .write(key, value)
    }

    pub fn plugin_entries(&self) -> Result<Vec<PluginEntry>> {
        self.0.fetch_all(
            &format!("SELECT {ENTRY_COLUMNS} FROM plugin_entries ORDER BY plugin_id"),
            [],
            PluginEntry::from_row,
        )
    }

    /// Keeps the registry check and the plugin's state operation in one store lock.
    pub fn with_installation<T>(
        &self,
        plugin_id: &str,
        installation_id: &str,
        still_admitted: impl FnOnce() -> bool,
        access: impl FnOnce(PluginData<'_>) -> T,
    ) -> Result<T> {
        let conn = self.0.conn();
        let enabled: bool = conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM plugin_entries
             WHERE plugin_id = ?1 AND installation_id = ?2 AND enabled = 1)",
            )?
            .query_row(params![plugin_id, installation_id], |row| row.get(0))?;
        if !enabled || !still_admitted() {
            return Err(StoreError::Permission(
                "the plugin installation is no longer active".into(),
            ));
        }
        Ok(access(PluginData {
            conn: &conn,
            plugin_id,
        }))
    }

    pub fn install_plugin_entry(
        &self,
        plugin_id: &str,
        name: &str,
        version: &str,
        path: &str,
        revision: &str,
    ) -> Result<PluginEntry> {
        self.0.fetch(
            &format!(
                "INSERT INTO plugin_entries ({ENTRY_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5, ?6, lower(hex(randomblob(16))))
                 ON CONFLICT(plugin_id) DO UPDATE SET name = excluded.name, version = excluded.version,
                 path = excluded.path, enabled = 1, updated_at = excluded.updated_at,
                 revision = excluded.revision, installation_id = excluded.installation_id
                 RETURNING {ENTRY_COLUMNS}"
            ),
            params![plugin_id, name, version, path, now(), revision],
            PluginEntry::from_row,
        )?.ok_or_else(|| StoreError::Storage("the plugin installation did not persist".into()))
    }

    pub fn pin_legacy_plugin_revision(&self, plugin_id: &str, revision: &str) -> Result<()> {
        self.0
            .conn()
            .prepare_cached(
                "UPDATE plugin_entries SET revision = ?2 WHERE plugin_id = ?1 AND revision IS NULL",
            )?
            .execute(params![plugin_id, revision])?;
        Ok(())
    }

    pub fn set_plugin_enabled(
        &self,
        plugin_id: &str,
        enabled: bool,
    ) -> Result<Option<PluginEntry>> {
        self.0.fetch(
            &format!(
                "UPDATE plugin_entries SET enabled = ?2, updated_at = ?3 WHERE plugin_id = ?1
             RETURNING {ENTRY_COLUMNS}"
            ),
            params![plugin_id, enabled, now()],
            PluginEntry::from_row,
        )
    }

    pub fn remove_plugin_entry(&self, plugin_id: &str) -> Result<Option<PluginEntry>> {
        self.0.write_tx(|transaction| {
            let existing = transaction
                .prepare_cached(&format!(
                    "DELETE FROM plugin_entries WHERE plugin_id = ?1 RETURNING {ENTRY_COLUMNS}"
                ))?
                .query_row([plugin_id], PluginEntry::from_row)
                .optional()?;
            if existing.is_some() {
                transaction
                    .prepare_cached("DELETE FROM plugin_config WHERE plugin_id = ?1")?
                    .execute([plugin_id])?;
                transaction
                    .prepare_cached("DELETE FROM plugin_state WHERE plugin_id = ?1")?
                    .execute([plugin_id])?;
            }
            Ok(existing)
        })
    }
}

impl FromRow for PluginEntry {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(PluginEntry {
            plugin_id: row.get(0)?,
            name: row.get(1)?,
            version: row.get(2)?,
            path: row.get(3)?,
            enabled: row.get::<_, i64>(4)? != 0,
            installed_at: row.get(5)?,
            updated_at: row.get(6)?,
            revision: row.get(7)?,
            installation_id: row.get(8)?,
        })
    }
}

/// Configuration and state for one plugin under the repository's lock.
#[derive(Debug)]
pub struct PluginData<'a> {
    conn: &'a Connection,
    plugin_id: &'a str,
}

impl PluginData<'_> {
    pub fn config(&self) -> Result<Value> {
        let raw: Option<String> = self
            .conn
            .prepare_cached("SELECT config FROM plugin_config WHERE plugin_id = ?1")?
            .query_row([self.plugin_id], |row| row.get(0))
            .optional()?;
        match raw {
            Some(text) => serde_json::from_str(&text)
                .map_err(|error| StoreError::Storage(format!("corrupt plugin_config: {error}"))),
            None => Ok(serde_json::json!({})),
        }
    }

    pub fn read(&self, key: &str) -> Result<Option<Value>> {
        validate_state_key(key)?;
        let raw: Option<String> = self
            .conn
            .prepare_cached("SELECT value FROM plugin_state WHERE plugin_id = ?1 AND key = ?2")?
            .query_row(params![self.plugin_id, key], |row| row.get(0))
            .optional()?;
        raw.map(|text| {
            serde_json::from_str(&text)
                .map_err(|error| StoreError::Storage(format!("corrupt plugin_state: {error}")))
        })
        .transpose()
    }

    pub fn write(&self, key: &str, value: &Value) -> Result<()> {
        validate_state_key(key)?;
        let raw = serde_json::to_string(value)
            .map_err(|error| StoreError::Invalid(format!("plugin state JSON: {error}")))?;
        if raw.len() > MAX_VALUE_BYTES {
            return Err(StoreError::Invalid(
                "plugin state value exceeds 64 KiB limit".into(),
            ));
        }
        let conn = self.conn;
        let plugin_id = self.plugin_id;
        let exists: bool = conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM plugin_state WHERE plugin_id = ?1 AND key = ?2)",
            )?
            .query_row(params![plugin_id, key], |row| row.get(0))?;
        if !exists {
            let count: i64 = conn
                .prepare_cached("SELECT COUNT(*) FROM plugin_state WHERE plugin_id = ?1")?
                .query_row([plugin_id], |row| row.get(0))?;
            if count >= MAX_KEYS_PER_PLUGIN {
                return Err(StoreError::Invalid("plugin state key limit reached".into()));
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
        return Err(StoreError::Invalid(
            "plugin state key must be 1-128 characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_entries_config_and_state_round_trip_and_cascade_on_remove() {
        let state = tempfile::tempdir().unwrap();
        let store = Store::open(&state.path().join("host.sqlite3")).unwrap();
        let repo = store.plugin_repo();

        let entry = repo
            .install_plugin_entry(
                "demo-tool",
                "Demo Tool",
                "0.2.0",
                "/plugins/demo-tool",
                "example-revision",
            )
            .unwrap();
        assert_eq!(entry.plugin_id, "demo-tool");
        assert!(entry.enabled);
        assert_eq!(repo.plugin_entries().unwrap().len(), 1);

        repo.set_plugin_config("demo-tool", &serde_json::json!({"mode": "strict"}))
            .unwrap();
        assert_eq!(
            repo.plugin_config("demo-tool").unwrap(),
            serde_json::json!({"mode": "strict"})
        );
        repo.plugin_state_write("demo-tool", "cursor", &serde_json::json!(42))
            .unwrap();
        assert_eq!(
            repo.plugin_state_read("demo-tool", "cursor").unwrap(),
            Some(serde_json::json!(42))
        );

        assert!(
            repo.set_plugin_enabled("demo-tool", false)
                .unwrap()
                .is_some()
        );
        assert!(!repo.plugin_entries().unwrap()[0].enabled);

        assert!(repo.remove_plugin_entry("demo-tool").unwrap().is_some());
        assert_eq!(repo.plugin_entries().unwrap().len(), 0);
        assert_eq!(
            repo.plugin_config("demo-tool").unwrap(),
            serde_json::json!({})
        );
        assert_eq!(repo.plugin_state_read("demo-tool", "cursor").unwrap(), None);
    }
}
