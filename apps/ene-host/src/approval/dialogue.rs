use super::*;

pub(crate) async fn policy_tool(
    actor: &mut Actor,
    dialogue: &Dialogue,
    args: &Map<String, Value>,
) -> Result<String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    let cards: Vec<_> = actor
        .approvals
        .values()
        .filter(|pending| pending.card.companion_id == dialogue.companion)
        .collect();
    if text("action") == "list" {
        return Ok(serde_json::to_string(
            &cards.iter().map(|card| &card.card).collect::<Vec<_>>(),
        )?);
    }
    let direct = dialogue
        .direct
        .as_deref()
        .ok_or_else(|| denied("approval requires a direct user message"))?;
    let quote = text("quote").trim();
    if quote.is_empty() || !direct.contains(quote) {
        return Err(denied("approval lacks a direct quotation"));
    }
    if text("action") == "add_text"
        && quote == direct.trim()
        && (quote.starts_with("ポリシーに追加:")
            || quote.starts_with("Policy:")
            || quote.trim_end_matches('。').ends_with("していいよ")
            || quote.trim_end_matches('。').ends_with("して構いません"))
    {
        let mut policy = actor
            .settings()
            .await?
            .policy
            .into_option()
            .unwrap_or_default();
        let addition = quote
            .strip_prefix("ポリシーに追加:")
            .or_else(|| quote.strip_prefix("Policy:"))
            .unwrap_or(quote);
        if addition.trim().is_empty() {
            return Err(invalid("missing policy text"));
        }
        let revision = policy.revision;
        policy.text.push('\n');
        policy.text.push_str(addition.trim());
        actor
            .put_policy(w::PolicyChange {
                mutation: Some(w::Mutation {
                    request_key: id(),
                    revision,
                    ..Default::default()
                })
                .into(),
                policy: Some(policy).into(),
                ..Default::default()
            })
            .await?;
        let mut db = actor.db.acquire().await?;
        store::audit(&mut db, "policy", "updated", "dialogue").await?;
        actor.emit(Payload::Notice(Box::new(w::Notice {
            companion_id: dialogue.companion.clone(),
            text: "自由文ポリシーを更新しました。".into(),
            ..Default::default()
        })));
        return Ok("Policy updated.".into());
    }
    let action = text("action");
    let normalized = direct
        .trim()
        .trim_end_matches(['。', '!', '！', '.'])
        .to_lowercase();
    let direct_choice = match normalized.as_str() {
        "承認" | "いいよ" | "いいよ、進めて" | "進めて" | "approve" | "yes" | "go ahead" => {
            Choice::Allow
        }
        "拒否" | "やめて" | "deny" | "no" => Choice::Deny,
        _ => {
            let (prefix, target) = normalized
                .split_once(' ')
                .ok_or_else(|| denied("approval was not explicit"))?;
            if target != text("id") {
                return Err(denied("approval target is ambiguous"));
            }
            match prefix {
                "approve" | "承認" => Choice::Allow,
                "deny" | "拒否" => Choice::Deny,
                _ => return Err(denied("approval was not explicit")),
            }
        }
    };
    if (action == "allow" && direct_choice != Choice::Allow)
        || (action == "deny" && direct_choice != Choice::Deny)
        || !matches!(action, "allow" | "deny")
    {
        return Err(denied("approval choice differs from direct input"));
    }
    let pending = cards
        .iter()
        .find(|pending| {
            pending.card.id == text("id")
                && (cards.len() == 1 || normalized.contains(&pending.card.id))
        })
        .ok_or_else(|| denied("approval target is ambiguous"))?;
    actor
        .answer_from(
            w::ApprovalAnswer {
                mutation: crate::chat::mutation(),
                id: pending.card.id.clone(),
                digest: pending.card.digest.clone(),
                choice: direct_choice.into(),
                ..Default::default()
            },
            "dialogue",
        )
        .await?;
    Ok("Approval recorded.".into())
}
