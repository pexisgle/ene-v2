//! Companions and their mind: persistence and revision checks.

use crate::{Store, error::Result, json_column, require, to_json, util::now};
use ene_mind::{Companion, Profile};
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};

const INITIAL_ENE_SEED: &str = "default_companion_ene";
const PREVIOUS_ENE_PERSONA: &str = "一人称は「私」、ユーザーのことは「マスター」（別の呼び方を望まれたらそちらに従う）と呼ぶ。明るい敬語（〜ですよ、〜ですね、〜じゃないですか！）をベースに、親しみやすい軽口や茶目っ気を少し混ぜてテンポよく1〜3文で話す。相手を傷つける煽りや邪魔はしない。作業報告では最初に成否を簡潔に伝え、詳しい説明を求められたときは手順や根拠を丁寧に並べる。";

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

pub const COLUMNS: &str = "id, name, mind, profile, profile_revision, state, \
     state_revision, body, voice, assignments, persona, voice_identifier, revision, \
     created_at, updated_at";

/// Repository managing `companions` and bootstrap seed state.
#[derive(Clone, Copy, Debug)]
pub struct CompanionRepo<'a>(&'a Store);

impl<'a> CompanionRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    pub fn companions(&self) -> Result<Vec<Companion>> {
        self.0.fetch_all(
            &format!("SELECT {COLUMNS} FROM companions ORDER BY created_at, id"),
            [],
            row_to_companion,
        )
    }

    pub fn companion(&self, id: &str) -> Result<Option<Companion>> {
        self.0.fetch(
            &format!("SELECT {COLUMNS} FROM companions WHERE id = ?1"),
            [id],
            row_to_companion,
        )
    }

    pub fn insert_companion(&self, companion: &Companion) -> Result<()> {
        let connection = self.0.conn();
        insert_companion_with(&connection, companion)
    }

    /// Seeds the initial `ene` companion once on a fresh host installation.
    ///
    /// Recording a one-time marker in `bootstrap_state` ensures that user edits
    /// or deletions are preserved across restarts instead of resurrecting `ene`.
    pub fn seed_initial_companion(&self) -> Result<bool> {
        let mut connection = self.0.conn();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stamp = now();
        let applied: Option<String> = transaction
            .query_row(
                "SELECT applied_at FROM bootstrap_state WHERE key = ?1",
                [INITIAL_ENE_SEED],
                |row| row.get(0),
            )
            .optional()?;
        let first_instruction_update = transaction.execute(
            "INSERT OR IGNORE INTO bootstrap_state (key, applied_at) VALUES (?1, ?2)",
            params!["default_ene_instructions_v2", stamp],
        )? != 0;
        if let Some(seed_stamp) = applied {
            if first_instruction_update {
                refresh_initial_ene_instructions(&transaction, &seed_stamp, &stamp)?;
            }
            transaction.commit()?;
            return Ok(false);
        }
        transaction.execute(
            "INSERT INTO bootstrap_state (key, applied_at) VALUES (?1, ?2)",
            params![INITIAL_ENE_SEED, stamp],
        )?;
        let count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM companions", [], |row| row.get(0))?;
        let inserted = if count == 0 {
            let companion = ene_mind::default_ene_companion(&stamp);
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
    pub fn update_companion(&self, companion: &Companion, expected: u64) -> Result<()> {
        let updated = self
            .0
            .conn()
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

    pub fn update_mind_profile(
        &self,
        companion_id: &str,
        expected: u64,
        profile: &Profile,
    ) -> Result<u64> {
        let next = expected + 1;
        let updated = self
            .0
            .conn()
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

    pub fn delete_companion(&self, id: &str) -> Result<bool> {
        let deleted = self
            .0
            .conn()
            .prepare_cached("DELETE FROM companions WHERE id = ?1")?
            .execute([id])?;
        Ok(deleted > 0)
    }
}

fn refresh_initial_ene_instructions(
    connection: &rusqlite::Connection,
    seed_stamp: &str,
    stamp: &str,
) -> Result<()> {
    let defaults = ene_mind::default_ene_companion(stamp);
    connection.execute(
        "UPDATE companions SET
            profile = CASE WHEN profile_revision = 1 THEN
                json_set(profile, '$.facets', json(?1), '$.expression', json(?2)) ELSE profile END,
            profile_revision = profile_revision + (profile_revision = 1),
            persona = CASE WHEN persona = ?3 THEN ?4 ELSE persona END,
            revision = revision + CASE WHEN persona = ?3 THEN 1 ELSE 0 END,
            updated_at = ?5
         WHERE id = 'ene' AND created_at = ?6 AND (profile_revision = 1 OR persona = ?3)",
        params![
            to_json(&defaults.profile.facets)?,
            to_json(&defaults.profile.expression)?,
            PREVIOUS_ENE_PERSONA,
            defaults.persona,
            stamp,
            seed_stamp,
        ],
    )?;
    Ok(())
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
        let repo = store.companion_repo();

        assert!(repo.seed_initial_companion().unwrap());
        let seeded = repo
            .companion("ene")
            .unwrap()
            .expect("default ene companion must be seeded");
        assert_eq!(seeded.name, "ene");
        assert_eq!(seeded.body.schema, "ene.body.v1");
        assert_eq!(seeded.body.json["model"], "ENE.vrm");
        assert_ne!(seeded.profile.facets.len(), 0);
        assert_ne!(seeded.profile.examples.len(), 0);

        assert!(!repo.seed_initial_companion().unwrap());
        assert!(repo.delete_companion("ene").unwrap());
        assert!(!repo.seed_initial_companion().unwrap());
        assert_eq!(repo.companions().unwrap().len(), 0);
    }

    #[test]
    fn startup_refreshes_only_the_original_ene_instructions() {
        let seed_stamp = "2026-01-01T00:00:00Z";
        let defaults = ene_mind::default_ene_companion(seed_stamp);
        for (original, edited_profile, edited_persona) in [
            (true, false, false),
            (true, true, false),
            (true, false, true),
            (false, false, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(&dir.path().join("host.sqlite3")).unwrap();
            let repo = store.companion_repo();
            store
                .conn()
                .execute(
                    "INSERT INTO bootstrap_state (key, applied_at) VALUES (?1, ?2)",
                    params![INITIAL_ENE_SEED, seed_stamp],
                )
                .unwrap();

            let mut before = defaults.clone();
            before.profile.facets = vec!["以前の指示".into()];
            before.profile.expression = vec!["以前の仕草".into()];
            before.profile_revision = if edited_profile { 2 } else { 1 };
            before.persona = Some(
                if edited_persona {
                    "独自の話し方"
                } else {
                    PREVIOUS_ENE_PERSONA
                }
                .into(),
            );
            before.assignments.companion.model_id = "selected-model".into();
            before.revision = 4;
            if !original {
                before.created_at = "2026-02-01T00:00:00Z".into();
            }
            repo.insert_companion(&before).unwrap();

            assert!(!repo.seed_initial_companion().unwrap());
            let after = repo.companion("ene").unwrap().unwrap();
            let mut expected = before;
            if original && !edited_profile {
                expected.profile.facets = defaults.profile.facets.clone();
                expected.profile.expression = defaults.profile.expression.clone();
                expected.profile_revision += 1;
            }
            if original && !edited_persona {
                expected.persona = defaults.persona.clone();
                expected.revision += 1;
            }
            if original {
                assert_ne!(after.updated_at, seed_stamp);
                expected.updated_at.clone_from(&after.updated_at);
            }
            assert_eq!(after, expected);
            assert!(!repo.seed_initial_companion().unwrap());
            assert_eq!(repo.companion("ene").unwrap().unwrap(), after);
        }
    }
}
