use super::*;
use crate::{actor::now, schedule::ScheduleCommands as _};
use ene_core::mind::{Message, Role};

#[tokio::test]
async fn schedule_survives_restart_and_recognizes_a_durable_reply() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = crate::store::open(root.path()).await?;
    sqlx::query("INSERT INTO schedules VALUES('reminder','ene','remind me',NULL,'UTC',?)")
        .bind(now() - 1)
        .execute(&store)
        .await?;
    store.close().await;

    let (host, running) = start(root.path()).await?;
    let turn = host
        .call(|actor| {
            Box::pin(async move {
                actor.tick_schedule().await?;
                assert!(actor.turns.is_empty(), "there is no dialogue model");
                let turn =
                    sqlx::query_scalar::<_, String>("SELECT turn_id FROM schedule_deliveries")
                        .fetch_one(&actor.db)
                        .await?;
                Ok(turn)
            })
        })
        .await?;
    host.shutdown.cancel();
    running.await?;

    let (host, running) = start(root.path()).await?;
    host.call(move |actor| {
        Box::pin(async move {
            let stored = sqlx::query_scalar::<_, String>("SELECT turn_id FROM schedule_deliveries")
                .fetch_one(&actor.db)
                .await?;
            assert_eq!(stored, turn);
            actor
                .append_message(
                    "ene",
                    Message {
                        id: "completed-message".into(),
                        turn_id: turn,
                        role: Role::Assistant,
                        text: "Scheduled reminder".into(),
                        time: now(),
                    },
                    None,
                )
                .await?;
            Ok(())
        })
    })
    .await?;
    host.shutdown.cancel();
    running.await?;

    let (host, running) = start(root.path()).await?;
    host.call(|actor| {
        Box::pin(async move {
            sqlx::query("UPDATE schedule_deliveries SET ready_at=0")
                .execute(&actor.db)
                .await?;
            actor.tick_schedule().await?;
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM schedules")
                    .fetch_one(&actor.db)
                    .await?,
                0
            );
            assert_eq!(actor.journal("ene").await?.entries.len(), 1);
            Ok(())
        })
    })
    .await?;
    host.shutdown.cancel();
    running.await?;
    Ok(())
}

#[tokio::test]
async fn changing_one_companions_device_leaves_the_other_subscription_live() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (host, running) = start(root.path()).await?;
    host.call(|actor| {
        Box::pin(async move {
            let mut other = actor.companion("ene").await?;
            other.id = "other".into();
            actor
                .put_companion(w::CompanionChange {
                    mutation: mutation("other-companion", 0),
                    companion: Some(other).into(),
                    ..Default::default()
                })
                .await?;
            for device in ["one", "two"] {
                actor
                    .register_device(w::DeviceChange {
                        mutation: mutation(device, 0),
                        device: Some(w::Device {
                            id: device.into(),
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    })
                    .await?;
            }
            let first = actor.activate(activation("ene", "one", "first")).await?;
            let other = actor.activate(activation("other", "two", "second")).await?;
            let changed = actor.activate(activation("ene", "two", "third")).await?;
            assert!(actor.active("one", first.generation).is_err());
            assert_eq!(actor.active("two", other.generation)?.companion_id, "other");
            assert_eq!(actor.active("two", changed.generation)?.companion_id, "ene");
            Ok(())
        })
    })
    .await?;
    host.shutdown.cancel();
    running.await?;
    let (host, running) = start(root.path()).await?;
    let snapshot = host.call(|actor| Box::pin(actor.snapshot())).await?;
    assert_eq!(snapshot.activations.len(), 2);
    assert!(
        snapshot
            .activations
            .iter()
            .all(|entry| entry.device_id == "two")
    );
    host.shutdown.cancel();
    running.await?;
    Ok(())
}

fn activation(companion: &str, device: &str, key: &str) -> w::ActivationRequest {
    w::ActivationRequest {
        companion_id: companion.into(),
        device_id: device.into(),
        mutation: mutation(key, 0),
        ..Default::default()
    }
}
