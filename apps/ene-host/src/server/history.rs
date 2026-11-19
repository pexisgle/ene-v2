//! Offline reset of learned state, serialized by the host's state lock.
use crate::host::{db::Store, mind::State};
use std::{error::Error, fs, io, path::Path};

pub(super) fn reset(state: &Path, requested: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
    let pending = state.join("rig-reset-pending");
    let already_pending = directory_exists(&pending)?;
    if !requested && !already_pending {
        return Ok(());
    }

    let store = Store::open(&state.join("host.sqlite3"))?;
    let active = state.join("rig");
    let active_exists = directory_exists(&active)?;
    if already_pending && active_exists {
        return Err(io::Error::other("active and pending history directories both exist").into());
    }
    if !already_pending {
        // A restart completes the reset before any agent opens a new log.
        if active_exists {
            fs::rename(&active, &pending)?;
        } else {
            fs::create_dir(&pending)?;
        }
    }

    let mut connection = store.conn()?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "DELETE FROM memory_edges;
         DELETE FROM memory_injections;
         DELETE FROM mind_operations;
         DELETE FROM memories;
         DELETE FROM device_operations;
         DELETE FROM approval_requests;
         DELETE FROM work_runs;
         DELETE FROM work_sessions;",
    )?;
    let empty = crate::host::db::to_json(&State::default())?;
    transaction.execute(
        "UPDATE companions SET state = ?1, state_revision = state_revision + 1 WHERE state <> ?1",
        [empty],
    )?;
    transaction.commit()?;
    fs::remove_dir_all(pending)?;
    Ok(())
}

fn directory_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(io::Error::other(format!(
            "expected a directory at {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_history_reset_keeps_connections_and_policies() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path();
        let db = state.join("host.sqlite3");
        let store = Store::open(&db).unwrap();
        store.seed_initial_companion().unwrap();
        store
            .conn()
            .unwrap()
            .execute_batch(
                "INSERT INTO memories (id, scope, content, importance, recorded_at)
                 VALUES ('memory', 'ene', 'remembered detail', 5, '');
             INSERT INTO policy_rules VALUES ('policy', 'work.write', '*', 'deny', '');
             INSERT INTO connections (id, name, enabled, kind, config, credential_state, updated_at)
                 VALUES ('connection', 'Existing connection', 1, 'llm', '{}', 'registered', '');",
            )
            .unwrap();
        drop(store);

        let pending = state.join("rig-reset-pending");
        fs::create_dir(&pending).unwrap();
        fs::write(pending.join("old-log"), b"conversation history").unwrap();
        reset(state, false).unwrap();
        assert!(!pending.exists());

        let store = Store::open(&db).unwrap();
        let connection = store.conn().unwrap();
        let memories: i64 = connection
            .query_row("SELECT count(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        let settings: (String, String) = connection
            .query_row(
                "SELECT name, credential_state FROM connections WHERE id = 'connection'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(memories, 0);
        assert_eq!(
            settings,
            ("Existing connection".into(), "registered".into())
        );
        assert_eq!(
            connection
                .query_row("SELECT id FROM policy_rules", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "policy"
        );
        drop(connection);
        assert_eq!(store.companions().unwrap()[0].id, "ene");
    }
}
