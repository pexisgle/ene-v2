//! Saved policy rules and free-text policies.
use super::{Result, Store, enum_name_column};
use crate::host::error::HostError;
use crate::host::permission::{MAX_POLICY_TEXT_CHARS, PolicyRule, PolicyText};
use rusqlite::{Row, params};
use std::collections::HashSet;

fn row_to_rule(row: &Row<'_>) -> rusqlite::Result<PolicyRule> {
    let effect = enum_name_column(row, "effect", "unknown policy effect")?;
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
                return Err(HostError::Invalid(("rule tool name is required").into()));
            }
            if rule.pattern.is_empty() || rule.pattern.chars().count() > 4_000 {
                return Err(HostError::Invalid(("rule pattern is required").into()));
            }
        }
        let now = super::now();
        let mut connection = self.conn()?;
        let transaction = connection.transaction()?;
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
                        now.clone()
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
        transaction.commit()?;
        drop(connection);
        self.policy_rules()
    }
}

fn row_to_text(row: &Row<'_>) -> rusqlite::Result<PolicyText> {
    Ok(PolicyText {
        id: row.get("id")?,
        text: row.get("text")?,
        created_at: row.get("created_at")?,
    })
}

impl Store {
    pub(crate) fn policy_texts(&self) -> Result<Vec<PolicyText>> {
        self.fetch_all(
            "SELECT id, text, created_at FROM policy_texts ORDER BY created_at, id",
            [],
            row_to_text,
        )
    }

    /// Applies an add-or-update and remove batch, then returns the full list.
    pub(crate) fn apply_policy_texts(
        &self,
        add_or_update: &[PolicyText],
        remove_ids: &[String],
    ) -> Result<Vec<PolicyText>> {
        let mut removed = HashSet::new();
        for id in remove_ids {
            super::validate_id(id)?;
            removed.insert(id.as_str());
        }
        for text in add_or_update {
            super::validate_id(&text.id)?;
            if text.text.trim().is_empty() || text.text.chars().count() > MAX_POLICY_TEXT_CHARS {
                return Err(HostError::Invalid(
                    ("policy text must be 1-500 characters").into(),
                ));
            }
        }
        let now = super::now();
        let mut connection = self.conn()?;
        let transaction = connection.transaction()?;
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
                        now.clone()
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
        transaction.commit()?;
        drop(connection);
        self.policy_texts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_and_texts_round_trip_and_reject_invalid_rows() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        assert!(store.policy_rules().unwrap().is_empty());
        assert!(store.policy_texts().unwrap().is_empty());

        let rule = PolicyRule {
            id: "r1".into(),
            tool_name: "device_request".into(),
            pattern: "/tmp/x".into(),
            effect: crate::host::permission::PolicyEffect::Allow,
            created_at: String::new(),
        };
        let updated = store
            .apply_policy_rules(std::slice::from_ref(&rule), &[])
            .unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].id, rule.id);
        assert!(!updated[0].created_at.is_empty(), "the row is stamped");
        assert_eq!(store.policy_rules().unwrap(), updated);

        let text = PolicyText {
            id: "p1".into(),
            text: "外部への送信は常に確認する".into(),
            created_at: String::new(),
        };
        let updated = store
            .apply_policy_texts(std::slice::from_ref(&text), &[])
            .unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].text, text.text);
        assert!(!updated[0].created_at.is_empty(), "the row is stamped");
        assert_eq!(store.policy_texts().unwrap(), updated);

        // An update keeps the id and rewrites the text.
        let rewritten = PolicyText {
            text: "深夜の作業は確認する".into(),
            ..text
        };
        let updated = store
            .apply_policy_texts(std::slice::from_ref(&rewritten), &[])
            .unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].text, rewritten.text);
        let removed = store
            .apply_policy_texts(&[], std::slice::from_ref(&rewritten.id))
            .unwrap();
        assert!(removed.is_empty());
        assert!(store.policy_texts().unwrap().is_empty());

        for invalid in ["", "   ", &"x".repeat(MAX_POLICY_TEXT_CHARS + 1)] {
            let error = store
                .apply_policy_texts(
                    &[PolicyText {
                        id: "p2".into(),
                        text: (*invalid).to_owned(),
                        created_at: String::new(),
                    }],
                    &[],
                )
                .unwrap_err();
            assert!(matches!(error, HostError::Invalid(_)), "{invalid:?}");
        }
        let error = store
            .apply_policy_texts(&[], &["not an id!".into()])
            .unwrap_err();
        assert!(matches!(error, HostError::Invalid(_)));
    }
}
