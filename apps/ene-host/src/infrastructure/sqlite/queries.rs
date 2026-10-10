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
    let config = conversation::load(&tx)?;
    let mut rows: BTreeMap<(String, u64), Message> = BTreeMap::new();
    // Reserve all active replies and their user inputs before allocating any history.
    let mut query =
        tx.prepare("SELECT body FROM messages WHERE status IN ('queued','streaming')")?;
    for body in query.query_map([], |r| r.get::<_, String>(0))? {
        let message: Message = serde_json::from_str(&body?)?;
        if let Some(user) = message.in_reply_to.as_ref() {
            let body: Option<String> = tx
                .query_row(
                    "SELECT body FROM messages WHERE id=?1 AND companion_id=?2",
                    params![user, message.companion_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(body) = body {
                let user: Message = serde_json::from_str(&body)?;
                rows.insert((user.companion_id.clone(), user.sequence), user);
            }
        }
        rows.insert((message.companion_id.clone(), message.sequence), message);
    }
    drop(query);
    let mut message_cursors = BTreeMap::new();

    // Include worst-case cursor lengths; leave space for runtime capabilities and health.
    let cursors: BTreeMap<_, _> = config
        .companions
        .keys()
        .map(|id| (id, format!("{}:{id}", i64::MAX)))
        .collect();
    let metadata = ene_api_types::budget::encoded_len(
        &(&config, cursors),
        ene_api_types::budget::MAX_RESPONSE_BYTES,
    )
    .map_err(|_| Error::ResourceLimit)?;
    let active = rows
        .values()
        .map(|row| {
            ene_api_types::budget::encoded_len(row, ene_api_types::budget::MAX_RESPONSE_BYTES)
                .map_err(|_| Error::ResourceLimit)
                .map(|n| n + 1)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum::<usize>();
    let mut remaining = ene_api_types::budget::MAX_RESPONSE_BYTES
        .checked_sub(metadata.saturating_add(active).saturating_add(1024 * 1024))
        .ok_or(Error::ResourceLimit)?;
    let mut companions_left = config.companions.len();
    for companion in config.companions.values() {
        let share = (remaining / companions_left.max(1)).min(MAX_HISTORY_BYTES);
        let recent = page_budget(&tx, &companion.id, None, 50, share)?;
        for message in recent.items {
            let key = (message.companion_id.clone(), message.sequence);
            if let std::collections::btree_map::Entry::Vacant(entry) = rows.entry(key) {
                remaining = remaining.saturating_sub(
                    ene_api_types::budget::encoded_len(
                        &message,
                        ene_api_types::budget::MAX_RESPONSE_BYTES,
                    )
                    .map_err(|_| Error::ResourceLimit)?
                        + 1,
                );
                entry.insert(message);
            }
        }
        message_cursors.insert(companion.id.clone(), recent.next_cursor);
        companions_left -= 1;
    }
    let messages = rows.into_values().collect();
    let result = StoredSnapshot {
        host_id: meta(&tx, "host_id")?,
        environment_id: meta(&tx, "environment_id")?,
        companions: config.companions.into_values().collect(),
        settings: config.settings,
        messages,
        message_cursors,
    };
    tx.commit()?;
    Ok(result)
}
