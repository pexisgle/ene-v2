use super::*;

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
) -> Result<ene_rpc::History> {
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
    let mut more = false;
    for body in bodies {
        let message: Message = serde_json::from_str(&body?)?;
        if items.len() == limit {
            more = true;
            break;
        }
        items.push(message);
    }
    items.reverse();
    let next_cursor = if more {
        Some(format!(
            "{}:{}",
            items
                .first()
                .map(|m| m.sequence)
                .ok_or(Error::ResourceLimit)?,
            companion
        ))
    } else {
        None
    };
    Ok(ene_rpc::History {
        items,
        next_cursor,
        ..Default::default()
    })
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

    for companion in config.companions.values() {
        let recent = page(&tx, &companion.id, None, 50)?;
        for message in recent.items {
            rows.entry((message.companion_id.clone(), message.sequence))
                .or_insert(message);
        }
        message_cursors.insert(companion.id.clone(), recent.next_cursor);
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
