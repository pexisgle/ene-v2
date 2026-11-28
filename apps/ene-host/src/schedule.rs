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
struct Delivery {
    schedule_id: String,
    companion_id: String,
    instruction: String,
    turn_id: String,
    once: bool,
}

pub(crate) fn work_key(turn: &str) -> String {
    format!("schedule-work:{turn}")
}

#[derive(FromRow)]
struct Schedule {
    id: String,
    instruction: String,
    cron: Option<String>,
    zone: String,
    due: i64,
}

pub(crate) trait ScheduleCommands {
    async fn schedule_tool(&self, companion: &str, args: &Map<String, Value>) -> Result<String>;
    async fn tick_schedule(&mut self) -> Result<()>;
    async fn complete_schedule(&self, turn: &str) -> Result<()>;
}

impl ScheduleCommands for Actor {
    async fn schedule_tool(&self, companion: &str, args: &Map<String, Value>) -> Result<String> {
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

    async fn tick_schedule(&mut self) -> Result<()> {
        queue_due(&self.db, now()).await?;
        let rows: Vec<Delivery> = sqlx::query_as(
            "SELECT s.id AS schedule_id, s.companion_id, s.instruction, d.turn_id, s.cron IS NULL AS once
             FROM schedule_deliveries d JOIN schedules s ON s.id=d.schedule_id
             WHERE d.ready_at<=? ORDER BY d.ready_at, d.due LIMIT 32",
        )
        .bind(now())
        .fetch_all(&self.db)
        .await?;
        for delivery in rows {
            if self.turns.contains_key(&delivery.companion_id) {
                continue;
            }
            if deliver_schedule(self, delivery).await.is_err() {
                tracing::warn!("scheduled turn could not start");
            }
        }
        Ok(())
    }

    async fn complete_schedule(&self, turn: &str) -> Result<()> {
        let delivery = sqlx::query_as(
            "SELECT s.id AS schedule_id, s.companion_id, s.instruction, d.turn_id, s.cron IS NULL AS once
             FROM schedule_deliveries d JOIN schedules s ON s.id=d.schedule_id WHERE d.turn_id=?",
        )
        .bind(turn)
        .fetch_optional(&self.db)
        .await?;
        if let Some(delivery) = delivery {
            acknowledge(&self.db, delivery).await?;
        }
        Ok(())
    }
}

async fn deliver_schedule(actor: &mut Actor, delivery: Delivery) -> Result<()> {
    let delivered = actor
        .journal(&delivery.companion_id)
        .await?
        .entries
        .iter()
        .any(|message| {
            message.turn_id == delivery.turn_id && message.role == ene_core::mind::Role::Assistant
        })
        || sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM receipts WHERE key=?)")
            .bind(work_key(&delivery.turn_id))
            .fetch_one(&actor.db)
            .await?;
    if delivered {
        return acknowledge(&actor.db, delivery).await;
    }
    sqlx::query(
        "UPDATE schedule_deliveries
         SET ready_at=?+min(300,5*(1<<min(attempts,6))), attempts=attempts+1
         WHERE turn_id=?",
    )
    .bind(now())
    .bind(&delivery.turn_id)
    .execute(&actor.db)
    .await?;
    actor
        .schedule_event(
            &delivery.companion_id,
            delivery.turn_id,
            format!(
                "[Scheduled request] {}",
                truncate(&delivery.instruction, 8000)
            ),
        )
        .await
}

async fn queue_due(db: &sqlx::SqlitePool, now: i64) -> Result<()> {
    let mut tx = db.begin().await?;
    let rows: Vec<Schedule> = sqlx::query_as(
        "SELECT s.* FROM schedules s WHERE s.due<=?
         AND NOT EXISTS(SELECT 1 FROM schedule_deliveries d WHERE d.schedule_id=s.id)
         ORDER BY s.due LIMIT 32",
    )
    .bind(now)
    .fetch_all(&mut *tx)
    .await?;
    for row in rows {
        if let Some(cron) = row.cron.as_deref() {
            let due = next(cron, &row.zone, row.due)?;
            sqlx::query("UPDATE schedules SET due=? WHERE id=?")
                .bind(due)
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO schedule_deliveries(schedule_id,due,turn_id,ready_at) VALUES(?,?,?,?)",
        )
        .bind(row.id)
        .bind(row.due)
        .bind(id())
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn acknowledge(db: &sqlx::SqlitePool, delivery: Delivery) -> Result<()> {
    if delivery.once {
        sqlx::query("DELETE FROM schedules WHERE id=?")
            .bind(delivery.schedule_id)
            .execute(db)
            .await?;
    } else {
        sqlx::query("DELETE FROM schedule_deliveries WHERE turn_id=?")
            .bind(delivery.turn_id)
            .execute(db)
            .await?;
    }
    Ok(())
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
