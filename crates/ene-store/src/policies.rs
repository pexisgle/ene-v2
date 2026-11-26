//! Saved policy rules and free-text policies.

use crate::row::{FromRow, row_fields};
use crate::{
    Store, StoreError,
    error::Result,
    util::{now, validate_id},
};
use ene_policy::{MAX_POLICY_TEXT_CHARS, PolicyRule, PolicyText};
use rusqlite::params;
use std::collections::HashSet;

row_fields!(PolicyRule, RULE_COLUMNS {
    id,
    tool_name,
    pattern,
    effect => enum_name_column("unknown policy effect"),
    created_at,
});

row_fields!(
    PolicyText,
    TEXT_COLUMNS {
        id,
        text,
        created_at,
    }
);

/// Repository managing `policy_rules` and `policy_texts`.
#[derive(Clone, Copy, Debug)]
pub struct PolicyRepo<'a>(&'a Store);

impl<'a> PolicyRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    pub fn policy_rules(&self) -> Result<Vec<PolicyRule>> {
        self.0.fetch_all(
            &format!(
                "SELECT {RULE_COLUMNS} FROM policy_rules \
             ORDER BY created_at, id"
            ),
            [],
            PolicyRule::from_row,
        )
    }

    pub(crate) fn capability_rules(&self) -> Result<Vec<PolicyRule>> {
        self.0.fetch_all(
            &format!("SELECT {RULE_COLUMNS} FROM policy_rules WHERE tool_name IN ('client_capability', '*')"),
            [],
            PolicyRule::from_row,
        )
    }

    /// Applies an add-or-update and remove batch, then returns the full list.
    pub fn apply_policy_rules(
        &self,
        add_or_update: &[PolicyRule],
        remove_ids: &[String],
    ) -> Result<Vec<PolicyRule>> {
        let mut removed = HashSet::new();
        for id in remove_ids {
            validate_id(id)?;
            removed.insert(id.as_str());
        }
        for rule in add_or_update {
            validate_id(&rule.id)?;
            if rule.tool_name.trim().is_empty() || rule.tool_name.chars().count() > 128 {
                return Err(StoreError::Invalid("rule tool name is required".into()));
            }
            if rule.pattern.is_empty() || rule.pattern.chars().count() > 4_000 {
                return Err(StoreError::Invalid("rule pattern is required".into()));
            }
        }
        let stamp = now();
        self.0.write_tx(|transaction| {
            for rule in add_or_update {
                transaction
                    .prepare_cached(
                        "INSERT INTO policy_rules (id, tool_name, pattern, effect, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(id) DO UPDATE SET tool_name = excluded.tool_name, \
                     pattern = excluded.pattern, effect = excluded.effect",
                    )?
                    .execute(params![
                        rule.id,
                        rule.tool_name,
                        rule.pattern,
                        rule.effect.as_str(),
                        if rule.created_at.is_empty() {
                            stamp.clone()
                        } else {
                            rule.created_at.clone()
                        }
                    ])?;
            }
            for id in removed {
                transaction
                    .prepare_cached("DELETE FROM policy_rules WHERE id = ?1")?
                    .execute([id])?;
            }
            Ok(())
        })?;
        self.policy_rules()
    }

    pub fn policy_texts(&self) -> Result<Vec<PolicyText>> {
        self.0.fetch_all(
            &format!("SELECT {TEXT_COLUMNS} FROM policy_texts ORDER BY created_at, id"),
            [],
            PolicyText::from_row,
        )
    }

    /// Applies an add-or-update and remove batch, then returns the full list.
    pub fn apply_policy_texts(
        &self,
        add_or_update: &[PolicyText],
        remove_ids: &[String],
    ) -> Result<Vec<PolicyText>> {
        let mut removed = HashSet::new();
        for id in remove_ids {
            validate_id(id)?;
            removed.insert(id.as_str());
        }
        for text in add_or_update {
            validate_id(&text.id)?;
            if text.text.trim().is_empty() || text.text.chars().count() > MAX_POLICY_TEXT_CHARS {
                return Err(StoreError::Invalid(
                    "policy text must be 1-500 characters".into(),
                ));
            }
        }
        let stamp = now();
        self.0.write_tx(|transaction| {
            for text in add_or_update {
                transaction
                    .prepare_cached(
                        "INSERT INTO policy_texts (id, text, created_at) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT(id) DO UPDATE SET text = excluded.text",
                    )?
                    .execute(params![
                        text.id,
                        text.text,
                        if text.created_at.is_empty() {
                            stamp.clone()
                        } else {
                            text.created_at.clone()
                        }
                    ])?;
            }
            for id in removed {
                transaction
                    .prepare_cached("DELETE FROM policy_texts WHERE id = ?1")?
                    .execute([id])?;
            }
            Ok(())
        })?;
        self.policy_texts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_policy::PolicyEffect;

    #[test]
    fn rules_and_texts_round_trip_and_reject_invalid_rows() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let repo = store.policy_repo();
        assert_eq!(repo.policy_rules().unwrap().len(), 0);
        assert_eq!(repo.policy_texts().unwrap().len(), 0);

        let rule = PolicyRule {
            id: "r1".into(),
            tool_name: "device_request".into(),
            pattern: "/tmp/x".into(),
            effect: PolicyEffect::Allow,
            created_at: String::new(),
        };
        let updated = repo
            .apply_policy_rules(std::slice::from_ref(&rule), &[])
            .unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].id, rule.id);
        assert!(!updated[0].created_at.is_empty(), "the row is stamped");
        assert_eq!(repo.policy_rules().unwrap(), updated);

        let text = PolicyText {
            id: "p1".into(),
            text: "外部への送信は常に確認する".into(),
            created_at: String::new(),
        };
        let updated = repo
            .apply_policy_texts(std::slice::from_ref(&text), &[])
            .unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].text, text.text);
        assert!(!updated[0].created_at.is_empty(), "the row is stamped");
        assert_eq!(repo.policy_texts().unwrap(), updated);

        let rewritten = PolicyText {
            text: "深夜の作業は確認する".into(),
            ..text
        };
        let updated = repo
            .apply_policy_texts(std::slice::from_ref(&rewritten), &[])
            .unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].text, rewritten.text);
        let removed = repo
            .apply_policy_texts(&[], std::slice::from_ref(&rewritten.id))
            .unwrap();
        assert_eq!(removed.len(), 0);
        assert_eq!(repo.policy_texts().unwrap().len(), 0);

        for invalid in ["", "   ", &"x".repeat(MAX_POLICY_TEXT_CHARS + 1)] {
            let error = repo
                .apply_policy_texts(
                    &[PolicyText {
                        id: "p2".into(),
                        text: (*invalid).to_owned(),
                        created_at: String::new(),
                    }],
                    &[],
                )
                .unwrap_err();
            assert!(matches!(error, StoreError::Invalid(_)), "{invalid:?}");
        }
        let error = repo
            .apply_policy_texts(&[], &["not an id!".into()])
            .unwrap_err();
        assert!(matches!(error, StoreError::Invalid(_)));
    }
}
