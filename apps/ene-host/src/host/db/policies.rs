//! Saved policy rules.
use super::{Result, Store, enum_column};
use crate::host::error::HostError;
use crate::host::permission::{PolicyEffect, PolicyRule};
use rusqlite::{Row, params};
use std::collections::HashSet;

fn row_to_rule(row: &Row<'_>) -> rusqlite::Result<PolicyRule> {
    let effect = enum_column(row, "effect", "unknown policy effect", PolicyEffect::parse)?;
    Ok(PolicyRule {
        id: row.get("id")?,
        tool_name: row.get("tool_name")?,
        pattern: row.get("pattern")?,
        effect,
        created_at: row.get("created_at")?,
    })
}

impl Store {
    pub(crate) fn policy_rules(&self) -> Result<Vec<PolicyRule>> {
        self.fetch_all(
            "SELECT id, tool_name, pattern, effect, created_at FROM policy_rules \
             ORDER BY created_at, id",
            [],
            row_to_rule,
        )
    }

    /// Applies an add-or-update and remove batch, then returns the full list.
    pub(crate) fn apply_policy_rules(
        &self,
        add_or_update: &[PolicyRule],
        remove_ids: &[String],
    ) -> Result<Vec<PolicyRule>> {
        let mut removed = HashSet::new();
        for id in remove_ids {
            super::validate_id(id)?;
            removed.insert(id.as_str());
        }
        for rule in add_or_update {
            super::validate_id(&rule.id)?;
            if rule.tool_name.trim().is_empty() || rule.tool_name.chars().count() > 128 {
                return Err(HostError::invalid("rule tool name is required"));
            }
            if rule.pattern.is_empty() || rule.pattern.chars().count() > 4_000 {
                return Err(HostError::invalid("rule pattern is required"));
            }
        }
        let now = super::now();
        let mut connection = self.conn()?;
        let transaction = connection.transaction()?;
        for rule in add_or_update {
            transaction.execute(
                "INSERT INTO policy_rules (id, tool_name, pattern, effect, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET tool_name = excluded.tool_name, \
                 pattern = excluded.pattern, effect = excluded.effect",
                params![
                    rule.id,
                    rule.tool_name,
                    rule.pattern,
                    rule.effect.as_str(),
                    if rule.created_at.is_empty() {
                        now.clone()
                    } else {
                        rule.created_at.clone()
                    }
                ],
            )?;
        }
        for id in removed {
            transaction.execute("DELETE FROM policy_rules WHERE id = ?1", [id])?;
        }
        transaction.commit()?;
        drop(connection);
        self.policy_rules()
    }
}
