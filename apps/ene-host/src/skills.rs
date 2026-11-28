use crate::{actor::Actor, error::invalid};
use anyhow::Result;
use serde_json::{Map, Value};

pub(crate) trait SkillCommands {
    async fn skill(&self, companion: &str, args: &Map<String, Value>) -> Result<String>;
}

impl SkillCommands for Actor {
    async fn skill(&self, companion: &str, args: &Map<String, Value>) -> Result<String> {
        let text = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
        };
        match text("action") {
            "list" => {
                let rows = sqlx::query_as::<_, (String, String)>(
                    "SELECT name,overview FROM skills WHERE companion_id=? ORDER BY name",
                )
                .bind(companion)
                .fetch_all(&self.db)
                .await?;
                Ok(serde_json::to_string(&rows)?)
            }
            "get" => Ok(sqlx::query_scalar(
                "SELECT instructions FROM skills WHERE companion_id=? AND name=?",
            )
            .bind(companion)
            .bind(text("name"))
            .fetch_one(&self.db)
            .await?),
            "save" => {
                let name = text("name");
                let overview = text("overview");
                let instructions = text("instructions");
                if name.is_empty()
                    || name.len() > 128
                    || overview.len() > 600
                    || instructions.is_empty()
                    || instructions.len() > 8000
                {
                    return Err(invalid("skill exceeds size limits"));
                }
                let result = sqlx::query(
                    "INSERT INTO skills (companion_id,name,overview,instructions)
                    SELECT ?,?,?,? WHERE (SELECT count(*) FROM skills WHERE companion_id=?) < 64
                      OR EXISTS (SELECT 1 FROM skills WHERE companion_id=? AND name=?)
                    ON CONFLICT(companion_id,name) DO UPDATE SET overview=excluded.overview,instructions=excluded.instructions"
                ).bind(companion).bind(name).bind(overview).bind(instructions)
                    .bind(companion).bind(companion).bind(name).execute(&self.db).await?;
                if result.rows_affected() == 0 {
                    return Err(invalid("skill capacity reached"));
                }
                Ok("Skill saved.".into())
            }
            _ => Err(invalid("invalid skill action")),
        }
    }
}
