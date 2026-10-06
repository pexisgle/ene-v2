use super::*;

// Reserve response space for snapshot metadata and active replies outside the page.
const MAX_HISTORY_BYTES: usize = ene_api_types::budget::MAX_HISTORY_PAGE_BYTES;

pub(super) fn meta(db: &Connection, key: &str) -> Result<String> {
    db.query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
        r.get(0)
    })
    .map_err(Error::from)
}
pub fn parse_cursor(companion: &str, cursor: &str) -> Result<u64> {
    let (sequence, id) = cursor.split_once(':').ok_or(Error::InvalidCursor)?;
    let sequence: u64 = sequence.parse().map_err(|_| Error::InvalidCursor)?;
    if id != companion || sequence == 0 || sequence > i64::MAX as u64 {
        return Err(Error::InvalidCursor);
    }
    Ok(sequence)
}
pub(super) fn page(
    db: &Connection,
    companion: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page<Message>> {
    page_budget(db, companion, cursor, limit, MAX_HISTORY_BYTES - 1024)
}
fn page_budget(
    db: &Connection,
    companion: &str,
    cursor: Option<&str>,
    limit: usize,
    budget: usize,
) -> Result<Page<Message>> {
    let before = cursor.map(|c| parse_cursor(companion, c)).transpose()?;
    let mut query=db.prepare("SELECT body FROM messages WHERE companion_id=?1 AND (?2 IS NULL OR sequence<?2) ORDER BY sequence DESC LIMIT ?3")?;
    let bodies = query.query_map(
        params![
            companion,
            before.map(|value| value as i64),
            (limit + 1) as i64
        ],
        |r| r.get::<_, String>(0),
    )?;
    let mut items = Vec::new();
    let mut bytes = 2; // JSON array brackets.
    let mut more = false;
    let mut omitted = None;
    for body in bodies {
        let message: Message = serde_json::from_str(&body?)?;
        let size = ene_api_types::budget::encoded_len(&message, MAX_HISTORY_BYTES)
            .map_err(|_| Error::OversizedMessage)?
            + usize::from(!items.is_empty());
        if items.len() == limit || bytes + size > budget {
            // Every supported message fits individually. Reject an oversized
            // stored row instead of returning an empty page that cannot advance.
            if items.is_empty() && size > MAX_HISTORY_BYTES {
                return Err(Error::OversizedMessage);
            }
            omitted = Some(
                message
                    .sequence
                    .checked_add(1)
                    .filter(|n| *n <= i64::MAX as u64)
                    .ok_or(Error::ResourceLimit)?,
            );
            more = true;
            break;
        }
        bytes += size;
        items.push(message);
    }
    items.reverse();
    let next_cursor = if more {
        Some(format!(
            "{}:{}",
            items
                .first()
                .map(|m| m.sequence)
                .or(omitted)
                .ok_or(Error::ResourceLimit)?,
            companion
        ))
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

pub(super) fn snapshot(db: &mut Connection) -> Result<StoredSnapshot> {
    let tx = db.transaction()?;
    let companion: Companion = serde_json::from_str(&meta(&tx, "companion")?)?;
    let settings: Settings = serde_json::from_str(&meta(&tx, "settings")?)?;
    let mut messages: BTreeMap<u64, Message> = BTreeMap::new();
    {
        let mut query = tx.prepare(
            "SELECT body FROM messages WHERE companion_id=?1 AND status IN ('queued','streaming')",
        )?;
        for body in query.query_map([&companion.id], |r| r.get::<_, String>(0))? {
            let m: Message = serde_json::from_str(&body?)?;
            if let Some(user_id) = m.in_reply_to.as_ref() {
                let body: Option<String> = tx
                    .query_row(
                        "SELECT body FROM messages WHERE id=?1 AND companion_id=?2",
                        params![user_id, companion.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(body) = body {
                    let user: Message = serde_json::from_str(&body)?;
                    messages.insert(user.sequence, user);
                }
            }
            messages.insert(m.sequence, m);
        }
    }
    let reserved = ene_api_types::budget::encoded_len(
        &(&companion, &settings, &messages),
        ene_api_types::budget::MAX_RESPONSE_BYTES,
    )
    .map_err(|_| Error::ResourceLimit)?;
    let remaining = ene_api_types::budget::MAX_RESPONSE_BYTES
        .checked_sub(reserved.saturating_add(1024 * 1024))
        .ok_or(Error::ResourceLimit)?;
    let recent = page_budget(
        &tx,
        &companion.id,
        None,
        50,
        remaining.min(MAX_HISTORY_BYTES),
    )?;
    for message in recent.items {
        messages.entry(message.sequence).or_insert(message);
    }
    let result = StoredSnapshot {
        host_id: meta(&tx, "host_id")?,
        environment_id: meta(&tx, "environment_id")?,
        companion,
        settings,
        messages: messages.into_values().collect(),
        next_cursor: recent.next_cursor,
    };
    tx.commit()?;
    Ok(result)
}
