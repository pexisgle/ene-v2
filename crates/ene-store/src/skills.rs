//! Procedural skills repository (`skills` table).

use crate::{
    Store,
    error::{Result, StoreError},
    util::now,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

const MAX_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 500;
const MAX_INSTRUCTIONS_CHARS: usize = 16_000;
const MAX_SKILLS_PER_COMPANION: i64 = 128;

/// Reusable step-by-step procedure scoped to one companion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub updated_at: String,
}

/// Validates and normalizes a procedural skill slug (`[a-z0-9_-]`, 1-64 characters).
pub fn validate_skill_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_NAME_CHARS
        || trimmed
            .bytes()
            .any(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'))
    {
        return Err(StoreError::Invalid(
            "skill name must be 1-64 lowercase [a-z0-9_-] characters".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Repository for companion-scoped procedural skills.
#[derive(Clone, Copy, Debug)]
pub struct SkillRepo<'a>(&'a Store);

impl<'a> SkillRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    pub fn list_skills(&self, companion_id: &str) -> Result<Vec<Skill>> {
        self.0.fetch_all(
            "SELECT name, description, instructions, updated_at FROM skills \
             WHERE companion_id = ?1 ORDER BY name",
            [companion_id],
            |row| {
                Ok(Skill {
                    name: row.get(0)?,
                    description: row.get(1)?,
                    instructions: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            },
        )
    }

    pub fn read_skill(&self, companion_id: &str, name: &str) -> Result<Option<Skill>> {
        let name = validate_skill_name(name)?;
        self.0.fetch(
            "SELECT name, description, instructions, updated_at FROM skills \
             WHERE companion_id = ?1 AND name = ?2",
            params![companion_id, name],
            |row| {
                Ok(Skill {
                    name: row.get(0)?,
                    description: row.get(1)?,
                    instructions: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            },
        )
    }

    pub fn save_skill(
        &self,
        companion_id: &str,
        name: &str,
        description: &str,
        instructions: &str,
    ) -> Result<Skill> {
        let name = validate_skill_name(name)?;
        let description = description.trim();
        let instructions = instructions.trim();
        if description.is_empty() || description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(StoreError::Invalid(
                "skill description must be 1-500 characters".into(),
            ));
        }
        if instructions.is_empty() || instructions.chars().count() > MAX_INSTRUCTIONS_CHARS {
            return Err(StoreError::Invalid(
                "skill instructions must be 1-16000 characters".into(),
            ));
        }
        let conn = self.0.conn();
        let exists = conn
            .prepare_cached("SELECT 1 FROM skills WHERE companion_id = ?1 AND name = ?2")?
            .query_row(params![companion_id, &name], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            let count: i64 = conn
                .prepare_cached("SELECT COUNT(*) FROM skills WHERE companion_id = ?1")?
                .query_row([companion_id], |row| row.get(0))?;
            if count >= MAX_SKILLS_PER_COMPANION {
                return Err(StoreError::Invalid("companion skill limit reached".into()));
            }
        }
        let updated_at = now();
        conn.prepare_cached(
            "INSERT INTO skills (companion_id, name, description, instructions, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(companion_id, name) DO UPDATE SET description = excluded.description, \
             instructions = excluded.instructions, updated_at = excluded.updated_at",
        )?
        .execute(params![
            companion_id,
            &name,
            description,
            instructions,
            &updated_at
        ])?;
        Ok(Skill {
            name,
            description: description.to_owned(),
            instructions: instructions.to_owned(),
            updated_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_skill_names_and_crud_scoping() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        let repo = store.skill_repo();

        assert!(matches!(
            validate_skill_name("Invalid_Name"),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            validate_skill_name(" "),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(
            validate_skill_name("  release-check ").unwrap(),
            "release-check"
        );

        let created = repo
            .save_skill("ene", "release-check", "Release steps", "1. fmt\n2. test")
            .unwrap();
        assert_eq!(created.name, "release-check");

        // Upsert updates description and instructions without creating a second row.
        let updated = repo
            .save_skill("ene", "release-check", "Updated steps", "1. check")
            .unwrap();
        assert_eq!(updated.description, "Updated steps");
        assert_eq!(updated.instructions, "1. check");

        let ene_skills = repo.list_skills("ene").unwrap();
        assert_eq!(ene_skills.len(), 1);
        assert_eq!(ene_skills[0].description, "Updated steps");

        assert_eq!(
            repo.read_skill("ene", "release-check")
                .unwrap()
                .map(|s| s.instructions),
            Some("1. check".into())
        );
        assert!(repo.read_skill("other", "release-check").unwrap().is_none());
        assert_eq!(repo.list_skills("other").unwrap().len(), 0);
    }
}
