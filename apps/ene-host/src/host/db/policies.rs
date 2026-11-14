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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_upsert_and_remove_in_one_batch() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let mut rule = PolicyRule {
            id: "r1".into(),
            tool_name: "device_request".into(),
            pattern: "*".into(),
            effect: PolicyEffect::Allow,
            created_at: String::new(),
        };
        let rules = store.apply_policy_rules(&[rule.clone()], &[]).unwrap();
        assert_eq!(rules.len(), 1);
        assert!(!rules[0].created_at.is_empty());
        rule.effect = PolicyEffect::Deny;
        let rules = store.apply_policy_rules(&[rule], &[]).unwrap();
        assert_eq!(rules[0].effect, PolicyEffect::Deny);
        let rules = store.apply_policy_rules(&[], &["r1".to_owned()]).unwrap();
        assert!(rules.is_empty());
    }
}
