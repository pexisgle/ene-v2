use crate::{
    error::{ApiError, missing},
    journal::Journal,
    model::Models,
    store,
};
use anyhow::{Context, Result};
use connectrpc::ErrorCode;
use ene_rpc::{self as w, event::Payload, model_selection::Role};
use futures_util::future::BoxFuture;
use sqlx::SqlitePool;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

pub(crate) type Job = Box<dyn for<'a> FnOnce(&'a mut Actor) -> BoxFuture<'a, ()> + Send>;

#[derive(Clone)]
pub(crate) struct Host {
    send: mpsc::Sender<Job>,
    pub shutdown: CancellationToken,
    pub workspace: Option<std::sync::Arc<crate::workspace::Workspace>>,
}

impl Host {
    pub(crate) async fn call<T: Send + 'static>(
        &self,
        job: impl for<'a> FnOnce(&'a mut Actor) -> BoxFuture<'a, Result<T>> + Send + 'static,
    ) -> Result<T> {
        let (reply, receive) = oneshot::channel();
        self.send
            .send(Box::new(|actor| {
                Box::pin(async move {
                    let _closed = reply.send(job(actor).await);
                })
            }))
            .await
            .map_err(|_| ApiError(ErrorCode::Unavailable, "host is stopped"))?;
        receive
            .await
            .context("actor stopped while handling the request")?
    }
}

pub(crate) struct Turn {
    pub id: String,
    pub cancel: CancellationToken,
}

pub(crate) struct Actor {
    pub host: Host,
    pub db: SqlitePool,
    pub state: PathBuf,
    pub journals: HashMap<String, Journal>,
    pub models: Models,
    pub jobs: JoinSet<()>,
    pub turns: HashMap<String, Turn>,
    pub groups: HashMap<String, Option<String>>,
    pub work: HashMap<String, crate::work::Work>,
    pub approvals: BTreeMap<String, crate::approval::Pending>,
    pub reflecting: HashMap<String, u64>,
    pub reports: VecDeque<(String, String)>,
    pub cursor: w::Cursor,
    pub events: broadcast::Sender<w::Event>,
    pub activation: w::Activation,
    pub audio: broadcast::Sender<w::AudioOutput>,
    pub speech: CancellationToken,
    pub devices: HashMap<String, w::Device>,
    pub device_ops: HashMap<String, crate::device::Dispatch>,
    pub operations: broadcast::Sender<w::DeviceOperation>,
    pub observation: Option<(String, String)>,
    pub observation_digest: Option<String>,
    pub eye_generation: u64,
    pub device_grants: Vec<String>,
}

impl Actor {
    pub(crate) async fn start(state: PathBuf) -> Result<(Host, tokio::task::JoinHandle<()>)> {
        let db = store::open(&state).await?;
        let (send, mut receive) = mpsc::channel::<Job>(128);
        let host = Host {
            send,
            shutdown: CancellationToken::new(),
            workspace: crate::workspace::Workspace::from_env()?.map(std::sync::Arc::new),
        };
        let shutdown = host.shutdown.clone();
        let mut actor = Self {
            host: host.clone(),
            db: db.clone(),
            state,
            journals: HashMap::new(),
            models: Models::new(db)?,
            jobs: JoinSet::new(),
            turns: HashMap::new(),
            groups: HashMap::new(),
            work: HashMap::new(),
            approvals: BTreeMap::new(),
            reflecting: HashMap::new(),
            reports: VecDeque::new(),
            cursor: w::Cursor {
                epoch: id(),
                ..Default::default()
            },
            events: broadcast::channel(256).0,
            activation: w::Activation::default(),
            audio: broadcast::channel(64).0,
            speech: shutdown.child_token(),
            devices: HashMap::new(),
            device_ops: HashMap::new(),
            operations: broadcast::channel(64).0,
            observation: None,
            observation_digest: None,
            eye_generation: 0,
            device_grants: std::env::var("ENE_HOST_DEVICE_CAPABILITIES")
                .unwrap_or_else(|_| "body.motion,body.expression,desktop.input".into())
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
        };
        actor.restore().await?;
        let running = tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(1));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    job = receive.recv() => match job {
                        Some(job) => job(&mut actor).await,
                        None => break,
                    },
                    outcome = actor.jobs.join_next(), if !actor.jobs.is_empty() => {
                        if outcome.is_some_and(|result| result.is_err()) {
                            tracing::error!("background worker failed");
                        }
                    }
                    _ = timer.tick() => {
                        if actor.tick().await.is_err() {
                            tracing::warn!("schedule processing failed");
                        }
                    }
                }
            }
            receive.close();
            drop(receive);
            actor.approvals.clear();
            actor.device_ops.clear();
            if tokio::time::timeout(Duration::from_secs(5), async {
                while actor.jobs.join_next().await.is_some() {}
            })
            .await
            .is_err()
            {
                actor.jobs.shutdown().await;
            }
        });
        Ok((host, running))
    }

    async fn restore(&mut self) -> Result<()> {
        let mut db = self.db.acquire().await?;
        let companions = store::list::<w::Companion>(&mut db, "companion").await?;
        if companions.is_empty()
            && store::list::<w::Character>(&mut db, "character")
                .await?
                .is_empty()
        {
            let character = w::Character {
                id: "ene".into(),
                revision: 1,
                profile: Some(w::Profile {
                    name: "エネ".into(),
                    persona: "好奇心があり、穏やかなコンパニオン。".into(),
                    speech: "ユーザーと同じ言語で、自然な口調で話す。".into(),
                    ..Default::default()
                })
                .into(),
                avatar: "assets/ENE.vrm".into(),
                ..Default::default()
            };
            let companion = w::Companion {
                id: "ene".into(),
                revision: 1,
                character_id: character.id.clone(),
                name: "エネ".into(),
                ..Default::default()
            };
            let mut tx = self.db.begin().await?;
            store::put(&mut tx, "character", &character.id, 0, &character).await?;
            store::put(&mut tx, "companion", &companion.id, 0, &companion).await?;
            tx.commit().await?;
        }
        drop(db);
        for companion in companions {
            self.journal(&companion.id).await?;
            self.restore_summary(&companion.id).await?;
        }
        self.restore_work().await
    }

    pub(crate) async fn journal(&mut self, companion: &str) -> Result<&mut Journal> {
        if !self.journals.contains_key(companion) {
            let journal = Journal::open(
                self.state
                    .join("conversations")
                    .join(format!("{companion}.jsonl")),
            )
            .await?;
            self.journals.insert(companion.to_owned(), journal);
        }
        self.journals
            .get_mut(companion)
            .context("conversation journal not loaded")
    }

    pub(crate) async fn companion(&self, id: &str) -> Result<w::Companion> {
        let mut db = self.db.acquire().await?;
        store::get(&mut db, "companion", id)
            .await?
            .ok_or_else(|| missing("companion not found"))
    }

    pub(crate) async fn connection(&self, id: &str) -> Result<w::Connection> {
        let mut db = self.db.acquire().await?;
        store::get(&mut db, "connection", id)
            .await?
            .ok_or_else(|| missing("connection not found"))
    }

    pub(crate) async fn selection(
        &self,
        companion: &w::Companion,
        role: Role,
    ) -> Result<(w::ModelSelection, w::Connection)> {
        let selection = companion
            .models
            .iter()
            .find(|model| model.role == role)
            .cloned()
            .ok_or(ApiError(
                ErrorCode::FailedPrecondition,
                "model role has no provider",
            ))?;
        let connection = self.connection(&selection.connection_id).await?;
        Ok((selection, connection))
    }

    pub(crate) fn emit(&mut self, payload: Payload) {
        self.cursor.sequence += 1;
        let _no_receivers = self.events.send(w::Event {
            cursor: Some(self.cursor.clone()).into(),
            payload: Some(payload),
            ..Default::default()
        });
    }

    pub(crate) async fn snapshot(&self) -> Result<w::Snapshot> {
        let mut db = self.db.acquire().await?;
        Ok(w::Snapshot {
            cursor: Some(self.cursor.clone()).into(),
            characters: store::list(&mut db, "character").await?,
            companions: store::list(&mut db, "companion").await?,
            approvals: self
                .approvals
                .values()
                .filter(|pending| {
                    pending.card.device_id.is_empty()
                        || pending.card.device_id == self.activation.device_id
                })
                .map(|pending| pending.card.clone())
                .collect(),
            devices: self.devices.values().cloned().collect(),
            activation: Some(self.activation.clone()).into(),
            work: store::list(&mut db, "work").await?,
            ..Default::default()
        })
    }

    pub(crate) async fn announce_snapshot(&mut self) -> Result<()> {
        let mut snapshot = self.snapshot().await?;
        if let Some(cursor) = snapshot.cursor.as_option_mut() {
            cursor.sequence += 1;
        }
        self.emit(Payload::Snapshot(Box::new(snapshot)));
        Ok(())
    }

    pub(crate) fn report(&mut self, companion: String, message: String) {
        if self.reports.len() == 128 {
            self.reports.pop_front();
        }
        self.reports.push_back((companion, message));
    }

    async fn tick(&mut self) -> Result<()> {
        if let Some(index) = self
            .reports
            .iter()
            .position(|(companion, _)| !self.turns.contains_key(companion))
            && let Some((companion, text)) = self.reports.remove(index)
        {
            self.trigger(&companion, text).await?;
        }
        self.tick_schedule().await?;
        self.tick_work();
        self.expire_dispatches();
        Ok(())
    }
}

pub(crate) fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}
