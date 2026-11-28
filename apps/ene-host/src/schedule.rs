use crate::{
    actor::{Actor, id, now},
    error::invalid,
    model::truncate,
};
use anyhow::Result;
use croner::Cron;
use serde_json::{Map, Value, json};
use sqlx::FromRow;

#[derive(FromRow)]
struct Schedule {
    id: String,
    companion_id: String,
    instruction: String,
    cron: Option<String>,
    zone: String,
    due: i64,
}

impl Actor {
    pub(crate) async fn schedule_tool(
        &self,
        companion: &str,
        args: &Map<String, Value>,
    ) -> Result<String> {
        let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
        match text("action") {
            "list" => {
                let rows: Vec<Schedule> = sqlx::query_as(
                    "SELECT * FROM schedules WHERE companion_id=? ORDER BY due LIMIT 100",
                )
                .bind(companion)
                .fetch_all(&self.db)
                .await?;
                Ok(serde_json::to_string(&rows.iter().map(|row| json!({
                    "id": row.id, "instruction": row.instruction, "cron": row.cron, "zone": row.zone, "due": row.due,
                })).collect::<Vec<_>>())?)
            }
            "delete" => {
                sqlx::query("DELETE FROM schedules WHERE id=? AND companion_id=?")
                    .bind(text("id"))
                    .bind(companion)
                    .execute(&self.db)
                    .await?;
                Ok("Schedule removed.".into())
            }
            "create" => {
                let instruction = text("instruction");
                let cron = (!text("cron").is_empty()).then_some(text("cron"));
                let local = jiff::ZonedDateTime::now();
                let zone = if text("zone").is_empty() {
                    local.time_zone().iana_name().unwrap_or("UTC")
                } else {
                    text("zone")
                };
                let due = match cron {
                    Some(cron) => next(cron, zone, now())?,
                    None => text("at").parse::<jiff::Timestamp>()?.as_second(),
                };
                if due <= now() || instruction.trim().is_empty() || instruction.len() > 8_000 {
                    return Err(invalid(
                        "schedule requires a future date and an instruction",
                    ));
                }
                let id = id();
                sqlx::query("INSERT INTO schedules VALUES(?,?,?,?,?,?)")
                    .bind(&id)
                    .bind(companion)
                    .bind(instruction)
                    .bind(cron)
                    .bind(zone)
                    .bind(due)
                    .execute(&self.db)
                    .await?;
                Ok(id)
            }
            _ => Err(invalid("invalid schedule action")),
        }
    }

    pub(crate) async fn tick_schedule(&mut self) -> Result<()> {
        let now = now();
        let rows: Vec<Schedule> =
            sqlx::query_as("SELECT * FROM schedules WHERE due<=? ORDER BY due LIMIT 16")
                .bind(now)
                .fetch_all(&self.db)
                .await?;
        for row in rows {
            if self.turns.contains_key(&row.companion_id) {
                continue;
            }
            if let Some(cron) = row.cron {
                let due = next(&cron, &row.zone, now)?;
                sqlx::query("UPDATE schedules SET due=? WHERE id=?")
                    .bind(due)
                    .bind(row.id)
                    .execute(&self.db)
                    .await?;
            } else {
                sqlx::query("DELETE FROM schedules WHERE id=?")
                    .bind(row.id)
                    .execute(&self.db)
                    .await?;
            }
            self.schedule_event(
                &row.companion_id,
                format!("[Scheduled request] {}", truncate(&row.instruction, 8000)),
            )
            .await?;
        }
        Ok(())
    }
}

fn next(cron: &str, zone: &str, now: i64) -> Result<i64> {
    let date = jiff::ZonedDateTime::new(
        jiff::Timestamp::from_second(now)?,
        jiff::tz::TimeZone::get(zone)?,
    );
    Ok(cron
        .parse::<Cron>()?
        .find_next_occurrence(&date, false)?
        .timestamp()
        .as_second())
}
