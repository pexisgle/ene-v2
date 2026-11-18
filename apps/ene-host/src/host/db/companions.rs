//! Companions and their mind: persistence and revision checks.
use super::{Result, Store, json_column, now, require, to_json};
use crate::host::mind::{Companion, Profile};
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};

fn row_to_companion(row: &Row<'_>) -> rusqlite::Result<Companion> {
    Ok(Companion {
        id: row.get("id")?,
        name: row.get("name")?,
        mind: row.get("mind")?,
        profile: json_column(row, "profile", "stored profile is corrupt")?,
        profile_revision: row.get::<_, u64>("profile_revision")?,
        state: json_column(row, "state", "stored state is corrupt")?,
        state_revision: row.get::<_, u64>("state_revision")?,
        body: json_column(row, "body", "stored body is corrupt")?,
        voice: json_column(row, "voice", "stored voice is corrupt")?,
        assignments: json_column(row, "assignments", "stored assignments are corrupt")?,
        persona: row.get("persona")?,
        voice_identifier: row.get("voice_identifier")?,
        revision: row.get::<_, u64>("revision")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) const COLUMNS: &str = "id, name, mind, profile, profile_revision, state, \
     state_revision, body, voice, assignments, persona, voice_identifier, revision, \
     created_at, updated_at";

impl Store {
    pub(crate) fn companions(&self) -> Result<Vec<Companion>> {
        self.fetch_all(
            &format!("SELECT {COLUMNS} FROM companions ORDER BY created_at, id"),
            [],
            row_to_companion,
        )
    }

    pub(crate) fn companion(&self, id: &str) -> Result<Option<Companion>> {
        self.fetch(
            &format!("SELECT {COLUMNS} FROM companions WHERE id = ?1"),
            [id],
            row_to_companion,
        )
    }

    pub(crate) fn insert_companion(&self, companion: &Companion) -> Result<()> {
        let connection = self.conn()?;
        insert_companion_with(&connection, companion)
    }

    /// Seeds the initial `ene` companion once on a fresh host installation.
    ///
    /// Recording a one-time marker in `bootstrap_state` ensures that user edits
    /// or deletions are preserved across restarts instead of resurrecting `ene`.
    pub(crate) fn seed_initial_companion(&self) -> Result<bool> {
        const KEY: &str = "default_companion_ene";
        let mut connection = self.conn()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let applied: Option<String> = transaction
            .query_row(
                "SELECT applied_at FROM bootstrap_state WHERE key = ?1",
                [KEY],
                |row| row.get(0),
            )
            .optional()?;
        if applied.is_some() {
            return Ok(false);
        }
        let stamp = now();
        transaction.execute(
            "INSERT INTO bootstrap_state (key, applied_at) VALUES (?1, ?2)",
            params![KEY, stamp],
        )?;
        let count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM companions", [], |row| row.get(0))?;
        let inserted = if count == 0 {
            let companion = crate::host::mind::default_ene_companion(&stamp);
            companion.validate()?;
            insert_companion_with(&transaction, &companion)?;
            true
        } else {
            false
        };
        transaction.commit()?;
        Ok(inserted)
    }

    /// Saves an edited companion with an optimistic revision check.
    pub(crate) fn update_companion(&self, companion: &Companion, expected: u64) -> Result<()> {
        let updated = self
            .conn()?
            .prepare_cached(
                "UPDATE companions SET name = ?2, mind = ?3, profile = ?4, profile_revision = ?5, \
             state = ?6, state_revision = ?7, body = ?8, voice = ?9, assignments = ?10, \
             persona = ?11, voice_identifier = ?12, revision = ?13, updated_at = ?14 \
             WHERE id = ?1 AND revision = ?15",
            )?
            .execute(params![
                companion.id,
                companion.name,
                companion.mind,
                to_json(&companion.profile)?,
                companion.profile_revision,
                to_json(&companion.state)?,
                companion.state_revision,
                to_json(&companion.body)?,
                to_json(&companion.voice)?,
                to_json(&companion.assignments)?,
                companion.persona,
                companion.voice_identifier,
                companion.revision,
                companion.updated_at,
                expected,
            ])?;
        require(updated, "companion changed since it was read")
    }

    pub(crate) fn update_mind_profile(
        &self,
        companion_id: &str,
        expected: u64,
        profile: &Profile,
    ) -> Result<u64> {
        let next = expected + 1;
        let updated = self
            .conn()?
            .prepare_cached(
                "UPDATE companions SET profile = ?2, profile_revision = ?3, updated_at = ?4 \
             WHERE id = ?1 AND profile_revision = ?5",
            )?
            .execute(params![
                companion_id,
                to_json(profile)?,
                next,
                now(),
                expected
            ])?;
        require(updated, "profile changed since it was read")?;
        Ok(next)
    }

    pub(crate) fn delete_companion(&self, id: &str) -> Result<bool> {
        let deleted = self
            .conn()?
            .prepare_cached("DELETE FROM companions WHERE id = ?1")?
            .execute([id])?;
        Ok(deleted > 0)
    }
}

fn insert_companion_with(connection: &rusqlite::Connection, companion: &Companion) -> Result<()> {
    connection
        .prepare_cached(
            "INSERT INTO companions (id, name, mind, profile, profile_revision, state, \
         state_revision, body, voice, assignments, persona, voice_identifier, revision, \
         created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?
        .execute(params![
            companion.id,
            companion.name,
            companion.mind,
            to_json(&companion.profile)?,
            companion.profile_revision,
            to_json(&companion.state)?,
            companion.state_revision,
            to_json(&companion.body)?,
            to_json(&companion.voice)?,
            to_json(&companion.assignments)?,
            companion.persona,
            companion.voice_identifier,
            companion.revision,
            companion.created_at,
            companion.updated_at,
        ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_ene_companion_seeds_once_and_respects_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("host.sqlite3")).unwrap();

        assert!(store.seed_initial_companion().unwrap());
        let seeded = store
            .companion("ene")
            .unwrap()
            .expect("default ene companion must be seeded");
        assert_eq!(seeded.name, "ene");
        assert_eq!(seeded.body.schema, "ene.body.v1");
        assert_eq!(seeded.body.json["model"], "ENE.vrm");
        assert!(!seeded.profile.facets.is_empty());
        assert!(!seeded.profile.examples.is_empty());

        assert!(!store.seed_initial_companion().unwrap());
        assert!(store.delete_companion("ene").unwrap());
        assert!(!store.seed_initial_companion().unwrap());
        assert!(store.companions().unwrap().is_empty());
    }
}
