//! Companions and their mind: persistence and revision checks.
use super::{Result, Store, json_column, now, require, to_json};
#[cfg(test)]
use crate::host::mind::State;
use crate::host::mind::{Companion, Profile};
use rusqlite::{Row, params};

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
        connection.execute(
            "INSERT INTO companions (id, name, mind, profile, profile_revision, state, \
             state_revision, body, voice, assignments, persona, voice_identifier, revision, \
             created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
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
            ],
        )?;
        Ok(())
    }

    /// Saves an edited companion with an optimistic revision check.
    pub(crate) fn update_companion(&self, companion: &Companion, expected: u64) -> Result<()> {
        let updated = self.conn()?.execute(
            "UPDATE companions SET name = ?2, mind = ?3, profile = ?4, profile_revision = ?5, \
             state = ?6, state_revision = ?7, body = ?8, voice = ?9, assignments = ?10, \
             persona = ?11, voice_identifier = ?12, revision = ?13, updated_at = ?14 \
             WHERE id = ?1 AND revision = ?15",
            params![
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
            ],
        )?;
        require(updated, "companion changed since it was read")
    }

    /// Updates only the mind state, but only when the caller still has the
    /// revision it read.
    #[cfg(test)]
    pub(crate) fn update_mind_state(
        &self,
        companion_id: &str,
        expected: u64,
        state: &State,
    ) -> Result<u64> {
        let next = expected + 1;
        let updated = self.conn()?.execute(
            "UPDATE companions SET state = ?2, state_revision = ?3, updated_at = ?4 \
             WHERE id = ?1 AND state_revision = ?5",
            params![companion_id, to_json(state)?, next, now(), expected],
        )?;
        require(updated, "mind changed since it was read")?;
        Ok(next)
    }

    pub(crate) fn update_mind_profile(
        &self,
        companion_id: &str,
        expected: u64,
        profile: &Profile,
    ) -> Result<u64> {
        let next = expected + 1;
        let updated = self.conn()?.execute(
            "UPDATE companions SET profile = ?2, profile_revision = ?3, updated_at = ?4 \
             WHERE id = ?1 AND profile_revision = ?5",
            params![companion_id, to_json(profile)?, next, now(), expected],
        )?;
        require(updated, "profile changed since it was read")?;
        Ok(next)
    }

    pub(crate) fn delete_companion(&self, id: &str) -> Result<bool> {
        let deleted = self
            .conn()?
            .execute("DELETE FROM companions WHERE id = ?1", [id])?;
        Ok(deleted > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::mind::{Assignments, Companion};

    fn companion(id: &str) -> Companion {
        Companion {
            id: id.into(),
            name: "ene".into(),
            mind: "core".into(),
            profile: Profile::default(),
            profile_revision: 1,
            state: State::default(),
            state_revision: 1,
            body: crate::host::json::JsonDoc::new("ene.body.v1", serde_json::json!({})),
            voice: crate::host::json::JsonDoc::new("ene.voice.v1", serde_json::json!({})),
            assignments: Assignments::default(),
            persona: None,
            voice_identifier: None,
            revision: 1,
            created_at: super::now(),
            updated_at: super::now(),
        }
    }

    #[test]
    fn companions_round_trip_and_reject_stale_revisions() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let mut first = companion("c1");
        store.insert_companion(&first).unwrap();
        assert_eq!(store.companions().unwrap().len(), 1);
        first.name = "ene".into();
        assert!(store.update_companion(&first, 2).is_err());
        first.revision = 2;
        store.update_companion(&first, 1).unwrap();
        let state = State::default();
        assert!(store.update_mind_state("c1", 99, &state).is_err());
        assert_eq!(store.update_mind_state("c1", 1, &state).unwrap(), 2);
        assert!(store.delete_companion("c1").unwrap());
        assert!(store.companions().unwrap().is_empty());
    }
}
