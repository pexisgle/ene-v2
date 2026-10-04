use super::*;

// Reserve response space for snapshot metadata and active replies outside the page.
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;

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
    let before = cursor
        .map(|c| parse_cursor(companion, c))
        .transpose()?
        .unwrap_or(i64::MAX as u64);
    let mut query=db.prepare("SELECT body FROM messages WHERE companion_id=?1 AND sequence<?2 ORDER BY sequence DESC LIMIT ?3")?;
    let bodies = query.query_map(params![companion, before as i64, (limit + 1) as i64], |r| {
        r.get::<_, String>(0)
    })?;
    let mut items = Vec::new();
    let mut bytes = 2; // JSON array brackets.
    let mut more = false;
    for body in bodies {
        let message: Message = serde_json::from_str(&body?)?;
        let size = serde_json::to_vec(&message)?.len() + usize::from(!items.is_empty());
        if items.len() == limit || bytes + size > MAX_HISTORY_BYTES {
            // Every supported message fits individually. Reject an oversized
            // stored row instead of returning an empty page that cannot advance.
            if items.is_empty() {
                return Err(Error::OversizedMessage);
            }
            more = true;
            break;
        }
        bytes += size;
        items.push(message);
    }
    items.reverse();
    let next_cursor = if more {
        items
            .first()
            .map(|m| format!("{}:{}", m.sequence, companion))
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

pub(super) fn snapshot(db: &mut Connection) -> Result<StoredSnapshot> {
    let tx = db.transaction()?;
    let config = conversation::load(&tx)?;
    let mut messages = Vec::new();
    let mut message_cursors = BTreeMap::new();
    for companion in config.companions.values() {
        let recent = page(&tx, &companion.id, None, 50)?;
        let mut rows: BTreeMap<u64, Message> =
            recent.items.into_iter().map(|m| (m.sequence, m)).collect();
        let mut query = tx.prepare(
            "SELECT body FROM messages WHERE companion_id=?1 AND status IN ('queued','streaming')",
        )?;
        for body in query.query_map([&companion.id], |r| r.get::<_, String>(0))? {
            let m: Message = serde_json::from_str(&body?)?;
            rows.insert(m.sequence, m);
        }
        messages.extend(rows.into_values());
        message_cursors.insert(companion.id.clone(), recent.next_cursor);
    }
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
