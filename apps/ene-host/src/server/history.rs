use anyhow::{Result, ensure};
use std::{fs, io, path::Path};

const MARKER: &str = "reset-history";
const JOURNALS: &str = "conversations";
const RETIRED: &str = "conversations-reset-pending";

pub(crate) fn request(state: &Path) -> Result<()> {
    let marker = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(state.join(MARKER))?;
    marker.sync_all()?;
    Ok(())
}

pub(super) async fn reset_pending(state: &Path) -> Result<()> {
    let marker = state.join(MARKER);
    if !marker.try_exists()? {
        return Ok(());
    }
    let journals = state.join(JOURNALS);
    let retired = state.join(RETIRED);
    let active = directory_exists(&journals)?;
    let staged = directory_exists(&retired)?;
    ensure!(
        !(active && staged),
        "both active and staged conversation logs exist"
    );
    if active {
        fs::rename(&journals, &retired)?;
    }

    let db = crate::store::open(state).await?;
    let mut tx = db.begin().await?;
    sqlx::raw_sql(
        "DELETE FROM records WHERE kind IN ('work','mind');
         DELETE FROM memories;
         DELETE FROM skills;
         DELETE FROM reflection_checkpoints;
         DELETE FROM receipts WHERE key NOT LIKE 'schedule-work:%';",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&db)
        .await?;
    db.close().await;

    if active || staged {
        fs::remove_dir_all(retired)?;
    }
    fs::remove_file(marker)?;
    Ok(())
}

fn directory_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_dir(),
                "expected a history directory"
            );
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn history_reset_removes_learned_data_and_leaves_settings() -> Result<()> {
        let state = tempfile::tempdir()?;
        let path = state.path();
        fs::create_dir(path.join(JOURNALS))?;
        fs::write(path.join(JOURNALS).join("ene.jsonl"), "old conversation")?;
        let db = crate::store::open(path).await?;
        sqlx::raw_sql(
            "INSERT INTO records VALUES('mind','ene',1,'{}');
             INSERT INTO records VALUES('companion','ene',1,'{}');
             INSERT INTO schedules VALUES('keep','ene','future schedule',NULL,'UTC',1);",
        )
        .execute(&db)
        .await?;
        db.close().await;
        request(path)?;
        reset_pending(path).await?;
        reset_pending(path).await?;
        assert!(!path.join(JOURNALS).exists());
        assert!(!path.join(RETIRED).exists());

        let db = crate::store::open(path).await?;
        let records = sqlx::query_scalar::<_, String>("SELECT kind FROM records")
            .fetch_all(&db)
            .await?;
        assert_eq!(records, ["companion"]);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT id FROM schedules")
                .fetch_one(&db)
                .await?,
            "keep"
        );
        Ok(())
    }
}
