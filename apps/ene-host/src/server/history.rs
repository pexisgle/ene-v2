//! Offline reset of learned state, serialized by the host's state lock.
use crate::host::{agent::companion_agent_id, autonomy::conversation_has_occurrence};
use ene_store::Store;
use private_fs::PrivateDir;
use std::{error::Error, fs, io, path::Path};

pub(super) fn reset(state: &Path, requested: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
    let pending = state.join("rig-reset-pending");
    let already_pending = directory_exists(&pending)?;
    if !requested && !already_pending {
        return Ok(());
    }

    let active = state.join("rig");
    let active_exists = directory_exists(&active)?;
    if already_pending && active_exists {
        return Err(io::Error::other("active and pending history directories both exist").into());
    }
    let directory = PrivateDir::open(state)?;
    let store = Store::open_for_history_reset(&state.join("host.sqlite3"))?;
    if !already_pending {
        // A restart completes the reset before any agent opens a new log.
        if active_exists {
            fs::rename(&active, &pending)?;
        } else {
            fs::create_dir(&pending)?;
        }
    }

    directory.sync_all()?;
    if let Some(store) = store {
        reconcile_schedule_receipts(&store, &pending)?;
        store.reset_learned_state()?;
    }
    fs::remove_dir_all(pending)?;
    directory.sync_all()?;
    Ok(())
}

fn reconcile_schedule_receipts(
    store: &Store,
    history: &Path,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let schedules = store.schedule_repo();
    let mut pending = schedules.pending_schedule_occurrences()?;
    if pending.is_empty() {
        return Ok(());
    }
    let logs = rig_memory::DirStore::open(history)?;
    let now = jiff::Timestamp::now();
    pending.sort_unstable_by(|a, b| a.companion_id.cmp(&b.companion_id));
    for occurrences in pending.chunk_by(|a, b| a.companion_id == b.companion_id) {
        let id = companion_agent_id(&occurrences[0].companion_id);
        let conversation = rig_memory::load(&logs, &id)?;
        for occurrence in occurrences {
            if conversation_has_occurrence(&conversation, &occurrence.occurrence_key) {
                schedules.mark_occurrence_delivered(&occurrence.occurrence_key, now)?;
            }
        }
    }
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
    use rig_core::{
        conversation::{Author, Entry, RequestId},
        message::Message,
    };

    #[test]
    fn interrupted_history_reset_keeps_connections_and_policies() {
        const STORED_DETAIL: &[u8] = b"RecallFixturezq9v2p7m5n3w8t1a6";
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path();
        let db = state.join("host.sqlite3");
        let store = Store::open(&db).unwrap();
        store.companion_repo().seed_initial_companion().unwrap();
        store
            .conn()
            .execute_batch(
                "INSERT INTO memories (id, scope, content, importance, recorded_at)
                 VALUES ('memory', 'ene', 'RecallFixturezq9v2p7m5n3w8t1a6', 5, '');
             INSERT INTO policy_rules VALUES ('policy', 'work.write', '*', 'deny', '');
             INSERT INTO connections (id, name, enabled, kind, config, credential_state, updated_at)
                 VALUES ('connection', 'Existing connection', 1, 'llm', '{}', 'registered', '');
             INSERT INTO schedule_occurrences
                 (occurrence_key, schedule_id, companion_id, scheduled_for, title, instruction, status, created_at)
                 VALUES ('done', 'old', 'ene', '', '', 'RecallFixturezq9v2p7m5n3w8t1a6', 'delivered', ''),
                        ('rec', 'old', 'ene', '', '', 'Logged instruction', 'pending', ''),
                        ('next', 'existing', 'ene', '', '', 'Future instruction', 'pending', '');
             INSERT INTO skills VALUES ('ene', 'learned', 'Learned steps', 'Learned instructions', '');",
            )
            .unwrap();
        drop(store);

        let pending = state.join("rig-reset-pending");
        fs::create_dir(&pending).unwrap();
        let logs = rig_memory::DirStore::open(&pending).unwrap();
        let log_id = companion_agent_id("ene");
        let mut conversation = rig_memory::load(&logs, &log_id).unwrap();
        let author = Author {
            request: Some(RequestId("rec".into())),
            ..Author::plugin("ene-host")
        };
        conversation.splice(
            0..0,
            [Entry::new(Message::user("Logged instruction")).by(author.clone())],
            author,
        );
        rig_memory::save(&logs, &log_id, &mut conversation).unwrap();
        reset(state, false).unwrap();
        assert!(!pending.exists());

        let store = Store::open(&db).unwrap();
        let connection = store.conn();
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
        let occurrences: (i64, String) = connection
            .query_row(
                "SELECT count(*), min(instruction) FROM schedule_occurrences",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(occurrences, (1, "Future instruction".into()));
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM skills", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM memories_fts_data WHERE instr(block, ?) > 0",
                    [b"9v2".as_slice()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
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
        assert_eq!(store.companion_repo().companions().unwrap()[0].id, "ene");
        drop(store);
        let bytes = fs::read(&db).unwrap();
        assert!(
            !bytes
                .windows(STORED_DETAIL.len())
                .any(|part| part == STORED_DETAIL)
        );
    }
}
