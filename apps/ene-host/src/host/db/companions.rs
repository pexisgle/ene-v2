//! Companion rows.
use super::{Store, parse_json};
use crate::host::{
    db::Result,
    error::HostError,
    mind::{Assignments, Companion, ModelAssignment},
};
use rusqlite::{Connection, OptionalExtension, params};

const COLUMNS: &str = "id, name, mind, body, voice, companion_connection, companion_model, \
    task_connection, task_model, perception_connection, perception_model, \
    compaction_connection, compaction_model, consolidation_connection, consolidation_model, \
    decision_connection, decision_model, embedding_connection, embedding_model, profile, state, \
    persona, voice_identifier, revision, profile_revision, state_revision, memory_revision, \
    created_at, updated_at";

impl Store {
    pub(crate) fn companions(&self) -> Result<Vec<Companion>> {
        self.call(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {COLUMNS} FROM companions ORDER BY created_at, id"
            ))?;
            statement
                .query_map([], row_to_companion)?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn companion(&self, id: &str) -> Result<Option<Companion>> {
        let id = id.to_owned();
        self.call(move |connection| read_companion(connection, &id).map_err(Into::into))
    }

    pub(crate) fn save_companion(&self, companion: &Companion) -> Result<()> {
        let companion = companion.clone();
        self.call(move |connection| {
            write_companion(connection, &companion)?;
            Ok(())
        })
    }

    pub(crate) fn remove_companion(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.call(move |connection| {
            connection.execute("DELETE FROM companions WHERE id=?1", [&id])?;
            Ok(())
        })
    }
}

pub(super) fn read_companion(
    connection: &Connection,
    id: &str,
) -> rusqlite::Result<Option<Companion>> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM companions WHERE id=?1"),
            [id],
            row_to_companion,
        )
        .optional()
}

pub(super) fn write_companion(
    connection: &Connection,
    companion: &Companion,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO companions(id, name, mind, body, voice, companion_connection, \
         companion_model, task_connection, task_model, perception_connection, perception_model, \
         compaction_connection, compaction_model, consolidation_connection, consolidation_model, \
         decision_connection, decision_model, embedding_connection, embedding_model, profile, \
         state, persona, voice_identifier, revision, profile_revision, state_revision, \
         memory_revision, created_at, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29) \
         ON CONFLICT(id) DO UPDATE SET name=excluded.name, mind=excluded.mind, \
         body=excluded.body, voice=excluded.voice, companion_connection=excluded.companion_connection, \
         companion_model=excluded.companion_model, task_connection=excluded.task_connection, \
         task_model=excluded.task_model, perception_connection=excluded.perception_connection, \
         perception_model=excluded.perception_model, compaction_connection=excluded.compaction_connection, \
         compaction_model=excluded.compaction_model, consolidation_connection=excluded.consolidation_connection, \
         consolidation_model=excluded.consolidation_model, decision_connection=excluded.decision_connection, \
         decision_model=excluded.decision_model, embedding_connection=excluded.embedding_connection, \
         embedding_model=excluded.embedding_model, profile=excluded.profile, state=excluded.state, \
         persona=excluded.persona, voice_identifier=excluded.voice_identifier, \
         revision=excluded.revision, profile_revision=excluded.profile_revision, \
         state_revision=excluded.state_revision, memory_revision=excluded.memory_revision, \
         updated_at=excluded.updated_at",
        params![
            companion.id,
            companion.name,
            companion.mind,
            companion.body.to_string(),
            companion.voice.to_string(),
            text(companion.assignments.companion.as_ref().map(|a| a.connection.as_str())),
            text(companion.assignments.companion.as_ref().map(|a| a.model.as_str())),
            text(companion.assignments.task.as_ref().map(|a| a.connection.as_str())),
            text(companion.assignments.task.as_ref().map(|a| a.model.as_str())),
            text(companion.assignments.perception.as_ref().map(|a| a.connection.as_str())),
            text(companion.assignments.perception.as_ref().map(|a| a.model.as_str())),
            text(companion.assignments.compaction.as_ref().map(|a| a.connection.as_str())),
            text(companion.assignments.compaction.as_ref().map(|a| a.model.as_str())),
            text(companion.assignments.consolidation.as_ref().map(|a| a.connection.as_str())),
            text(companion.assignments.consolidation.as_ref().map(|a| a.model.as_str())),
            text(companion.assignments.decision.as_ref().map(|a| a.connection.as_str())),
            text(companion.assignments.decision.as_ref().map(|a| a.model.as_str())),
            text(companion.assignments.embedding.as_ref().map(|a| a.connection.as_str())),
            text(companion.assignments.embedding.as_ref().map(|a| a.model.as_str())),
            serde_json::to_string(&companion.profile).map_err(json_error)?,
            serde_json::to_string(&companion.state).map_err(json_error)?,
            companion.persona,
            companion.voice_identifier,
            companion.revision,
            companion.profile_revision,
            companion.state_revision,
            companion.memory_revision,
            companion.created_at,
            companion.updated_at,
        ],
    )?;
    Ok(())
}

fn text(value: Option<&str>) -> &str {
    value.unwrap_or("")
}

fn row_to_companion(row: &rusqlite::Row<'_>) -> rusqlite::Result<Companion> {
    let profile: String = row.get(19)?;
    let state: String = row.get(20)?;
    Ok(Companion {
        id: row.get(0)?,
        name: row.get(1)?,
        mind: row.get(2)?,
        body: json(row, 3)?,
        voice: json(row, 4)?,
        assignments: Assignments {
            companion: assignment(row.get(5)?, row.get(6)?),
            task: assignment(row.get(7)?, row.get(8)?),
            perception: assignment(row.get(9)?, row.get(10)?),
            compaction: assignment(row.get(11)?, row.get(12)?),
            consolidation: assignment(row.get(13)?, row.get(14)?),
            decision: assignment(row.get(15)?, row.get(16)?),
            embedding: assignment(row.get(17)?, row.get(18)?),
        },
        profile: serde_json::from_str(&profile).map_err(json_error)?,
        state: serde_json::from_str(&state).map_err(json_error)?,
        persona: row.get(21)?,
        voice_identifier: row.get(22)?,
        revision: row.get(23)?,
        profile_revision: row.get(24)?,
        state_revision: row.get(25)?,
        memory_revision: row.get(26)?,
        created_at: row.get(27)?,
        updated_at: row.get(28)?,
    })
}

fn json(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<serde_json::Value> {
    let value: String = row.get(index)?;
    parse_json(&value).map_err(json_error)
}

fn json_error(error: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(HostError::storage(error.to_string())),
    )
}

fn assignment(connection: String, model: String) -> Option<ModelAssignment> {
    (!connection.is_empty() && !model.is_empty()).then_some(ModelAssignment { connection, model })
}
