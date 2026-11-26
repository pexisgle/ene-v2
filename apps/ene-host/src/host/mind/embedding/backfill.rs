//! Background embedding backfill per companion.
use super::{Companion, Embeddings, index::encode, request, target};
use crate::host::{
    db::{Result, Store},
    error::HostError,
    model::Credentials,
};
use bevy_ecs::prelude::*;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

const BACKFILL_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BACKFILL: usize = 256;
pub(super) const MAX_EMBED_BATCH: usize = 16;

/// The companion ids with a backfill in flight and the failures to report.
#[derive(Resource)]
pub(crate) struct Backfill {
    running: HashSet<String>,
    last: Option<Instant>,
    sender: crossbeam_channel::Sender<(String, Result<usize>)>,
    receiver: crossbeam_channel::Receiver<(String, Result<usize>)>,
}

impl Default for Backfill {
    fn default() -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(16);
        Self {
            running: HashSet::new(),
            last: None,
            sender,
            receiver,
        }
    }
}

/// Embeds what is missing for each companion with a usable connection. Each
#[derive(bevy_ecs::system::SystemParam)]
pub(super) struct BackfillParams<'w> {
    store: Res<'w, Store>,
    runtime: Option<Res<'w, crate::host::resources::HostRuntime>>,
    credentials: Option<Res<'w, Credentials>>,
    embeddings: Res<'w, Embeddings>,
    state: ResMut<'w, Backfill>,
    notifier: Option<ResMut<'w, crate::host::notify::Notifier>>,
}

/// Starts at most one background backfill per companion without starving the
/// frame loop or opening concurrent writers per companion. Each background
/// task writes through its own SQLite connection opened from the store's path
/// (docs/design/mind.md §3.2), so the ECS thread's `Mutex<Connection>` is not
/// held while the background writes; the ECS thread only schedules and
/// reports.
pub(super) fn backfill(mut commands: Commands, params: BackfillParams<'_>) {
    let BackfillParams {
        store,
        runtime,
        credentials,
        embeddings,
        mut state,
        mut notifier,
    } = params;
    while let Ok((companion, result)) = state.receiver.try_recv() {
        state.running.remove(&companion);
        match result {
            Ok(written) if written > 0 => {
                tracing::debug!(companion = %companion, written, "embeddings stored");
            }
            Ok(_) => {}
            Err(error) => {
                if let Some(notifier) = notifier.as_mut() {
                    notifier.notice(
                        Some(&companion),
                        "embedding_failed",
                        &ene_host_core::util::truncate(&error.to_string(), 300),
                    );
                }
            }
        }
    }
    let now = Instant::now();
    if state
        .last
        .is_some_and(|last| now.duration_since(last) < BACKFILL_INTERVAL)
    {
        return;
    }
    state.last = Some(now);
    let companions = match store.companion_repo().companions() {
        Ok(companions) => companions,
        Err(error) => {
            if error.is_storage() {
                let message = error.to_string();
                commands.queue(move |world: &mut World| {
                    crate::host::fail(world, message);
                });
            }
            return;
        }
    };
    for companion in companions {
        if state.running.contains(&companion.id) {
            continue;
        }
        let Ok(Some(target)) = target(&store, credentials.as_deref(), &companion, &embeddings)
        else {
            continue;
        };
        match store
            .memory_repo()
            .memories_missing_embedding(&companion.id, &target.space, 1)
        {
            Ok(pending) if pending.is_empty() => continue,
            Ok(_) => {}
            Err(error) => {
                if error.is_storage() {
                    let message = error.to_string();
                    commands.queue(move |world: &mut World| {
                        crate::host::fail(world, message);
                    });
                    return;
                }
                continue;
            }
        }
        let companion_id = companion.id.clone();
        state.running.insert(companion_id.clone());
        let sender = state.sender.clone();
        let store = store.clone();
        // A world without the host runtime has nothing to embed on; the
        // running marker is cleared so the next tick retries.
        let Some(runtime) = runtime.as_deref().map(|runtime| runtime.0.clone()) else {
            state.running.remove(&companion.id);
            continue;
        };
        let credentials = credentials.as_deref().cloned();
        let embeddings = embeddings.clone();
        runtime.spawn(async move {
            // The ECS connection is not moved: the task opens its own
            // connection to the same database, with the same WAL pragmas, so
            // the store's lock is never held while it writes.
            let result = match store.open_secondary() {
                Ok(secondary) => {
                    fill(
                        &secondary,
                        &store,
                        &embeddings,
                        credentials.as_ref(),
                        &companion,
                    )
                    .await
                }
                Err(error) => Err(error.into()),
            };
            let _discarded = sender.send((companion_id, result));
        });
    }
}

pub(super) async fn fill(
    secondary: &ene_store::Store,
    store: &Store,
    embeddings: &Embeddings,
    credentials: Option<&Credentials>,
    companion: &Companion,
) -> Result<usize> {
    let memories = secondary.memory_repo();
    let mut written = 0;
    while written < MAX_BACKFILL {
        let Some(target) = target(store, credentials, companion, embeddings)? else {
            break;
        };
        let pending =
            memories.memories_missing_embedding(&companion.id, &target.space, MAX_EMBED_BATCH)?;
        if pending.is_empty() {
            break;
        }
        let texts: Vec<String> = pending.iter().map(|row| row.content.clone()).collect();
        let vectors = request(&target, &texts).await?;
        if vectors.len() != pending.len() {
            return Err(HostError::Unavailable(
                ("embedding endpoint returned the wrong number of vectors").into(),
            ));
        }
        for (row, vector) in pending.iter().zip(&vectors) {
            if vector.len() != target.dimensions {
                return Err(HostError::Unavailable(
                    ("embedding dimensions do not match the connection").into(),
                ));
            }
            if memories.write_embedding(
                &row.id,
                row.content_revision,
                &target.space,
                target.dimensions,
                &encode(vector),
            )? {
                written += 1;
            }
        }
    }
    Ok(written)
}
