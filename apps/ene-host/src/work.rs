use crate::device::DeviceCommands as _;

mod runner;

use runner::run_work;

use crate::{
    actor::{Actor, Host, Turn, id, now},
    approval::Operation,
    error::{conflict, denied, invalid, missing},
    mcp::Mcp,
    model::{Models, ToolOutput, truncate},
    store,
};
use anyhow::{Context, Result, ensure};
use ene_core::{
    mind::Prompt,
    policy::{Action, Effect, Target},
};
use ene_rpc::{self as w, event::Payload, model_selection::Role};
use rig_core::{
    completion::ToolDefinition,
    message::{ToolCall, ToolName},
};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::time::Instant;

pub(crate) struct Work {
    pub session: w::WorkSession,
    pub active: Option<Turn>,
    revision: i64,
    last_activity: Instant,
    warned: bool,
}

struct Tool {
    peer: Arc<Mcp>,
    spec: rmcp::model::Tool,
    effect: Effect,
    revision: String,
}

struct WorkContext {
    search: Option<w::Connection>,
    operation: Operation,
    tools: BTreeMap<String, Tool>,
    plugin_tools: BTreeMap<String, crate::plugin_tools::PluginTool>,
    uncertain: parking_lot::Mutex<std::collections::HashSet<String>>,
}

pub(crate) trait WorkCommands {
    async fn restore_work(&mut self) -> Result<()>;
    async fn send_work(&mut self, request: w::WorkInput) -> Result<w::Receipt>;
    fn start_work<'a>(
        &'a mut self,
        session_id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<()>>;
    async fn finish_work(
        &mut self,
        session_id: &str,
        run_id: &str,
        result: Result<String>,
    ) -> Result<()>;
    async fn stop_work(&mut self, request: w::StopRequest) -> Result<w::Empty>;
    async fn stop_session(&mut self, session: &str, scope: Option<&str>) -> Result<()>;
    fn tick_work(&mut self);
}

impl WorkCommands for Actor {
    async fn restore_work(&mut self) -> Result<()> {
        let mut db = self.db.acquire().await?;
        let rows = sqlx::query_as::<_, (String, i64, sqlx::types::Json<w::WorkSession>)>(
            "SELECT id,revision,body FROM records WHERE kind='work'",
        )
        .fetch_all(&mut *db)
        .await?;
        let mut queued = Vec::new();
        for (id, mut revision, body) in rows {
            let mut session = body.0;
            let mut changed = false;
            for run in &mut session.runs {
                if run.outcome == w::Outcome::Running {
                    run.outcome = w::Outcome::Unknown.into();
                    run.report = "ホストが再起動したため、完了を確認できません。".into();
                    changed = true;
                }
            }
            if changed {
                store::put(&mut db, "work", &id, revision, &session).await?;
                revision += 1;
            }
            if session
                .runs
                .iter()
                .any(|run| run.outcome == w::Outcome::Unspecified)
            {
                queued.push(id.clone());
                self.work.insert(
                    id,
                    Work {
                        session,
                        active: None,
                        revision,
                        last_activity: Instant::now(),
                        warned: false,
                    },
                );
            }
        }
        drop(db);
        for session in queued {
            self.start_work(&session).await?;
        }
        Ok(())
    }

    async fn send_work(&mut self, request: w::WorkInput) -> Result<w::Receipt> {
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("work", &request))?;
        let mut tx = self.db.begin().await?;
        if let Some(saved) = store::replay(&mut tx, &key.request_key, &hash).await? {
            return Ok(saved);
        }
        if request.instruction.trim().is_empty() || request.instruction.len() > 12000 {
            return Err(invalid("invalid work instruction"));
        }
        let companion = self.companion(&request.companion_id).await?;
        let session_id = if request.session_id.is_empty() {
            id()
        } else {
            request.session_id
        };
        crate::configuration::identifier(&session_id)?;
        let previous = sqlx::query_as::<_, (i64, sqlx::types::Json<w::WorkSession>)>(
            "SELECT revision,body FROM records WHERE kind='work' AND id=?",
        )
        .bind(&session_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (revision, mut session) = previous.map_or_else(
            || {
                (
                    0,
                    w::WorkSession {
                        id: session_id.clone(),
                        companion_id: companion.id.clone(),
                        ..Default::default()
                    },
                )
            },
            |(revision, body)| (revision, body.0),
        );
        if session.companion_id != companion.id {
            return Err(denied("work belongs to another companion"));
        }
        if session
            .runs
            .iter()
            .filter(|run| run.outcome == w::Outcome::Unspecified)
            .count()
            >= 16
        {
            return Err(conflict("work queue is full"));
        }
        let run = w::Run {
            id: id(),
            instruction: request.instruction,
            outcome: w::Outcome::Unspecified.into(),
            ..Default::default()
        };
        let receipt = w::Receipt {
            id: run.id.clone(),
            session_id: session_id.clone(),
            ..Default::default()
        };
        session.runs.push(run);
        store::put(&mut tx, "work", &session_id, revision, &session).await?;
        store::acknowledge(&mut tx, &key.request_key, &hash, &receipt).await?;
        tx.commit().await?;
        let work = self.work.entry(session_id.clone()).or_insert_with(|| Work {
            session: session.clone(),
            active: None,
            revision: 0,
            last_activity: Instant::now(),
            warned: false,
        });
        work.session = session;
        work.revision = revision + 1;
        if work.active.is_none() {
            self.start_work(&session_id).await?;
        }
        Ok(receipt)
    }

    fn start_work<'a>(
        &'a mut self,
        session_id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let work = self
                .work
                .get_mut(session_id)
                .ok_or_else(|| missing("work session unavailable"))?;
            let Some(index) = work
                .session
                .runs
                .iter()
                .position(|run| run.outcome == w::Outcome::Unspecified)
            else {
                self.work.remove(session_id);
                return Ok(());
            };
            let run = &mut work.session.runs[index];
            run.outcome = w::Outcome::Running.into();
            run.started_at = now();
            let cancel = self.host.shutdown.child_token();
            work.active = Some(Turn {
                id: run.id.clone(),
                cancel: cancel.clone(),
            });
            work.last_activity = Instant::now();
            work.warned = false;
            let operation = Operation {
                companion: work.session.companion_id.clone(),
                run: run.id.clone(),
                brief: run.instruction.clone(),
                cancel,
            };
            let history = work.session.runs[index.saturating_sub(4)..index]
                .iter()
                .map(|run| {
                    format!(
                        "Instruction: {}\nReport: {}",
                        truncate(&run.instruction, 1200),
                        truncate(&run.report, 1200)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let session = work.session.clone();
            let mut db = self.db.acquire().await?;
            store::put(&mut db, "work", session_id, work.revision, &session).await?;
            work.revision += 1;
            self.emit(Payload::Work(Box::new(session)));
            let companion = self.companion(&operation.companion).await?;
            let binding = self.selection(&companion, Role::Work).await;
            drop(db);
            let host = self.host.clone();
            let models = self.models.clone();
            let session_id = session_id.to_owned();
            self.jobs.spawn(async move {
                let result =
                    run_work(&host, &models, &operation, binding, &history, &session_id).await;
                let _closed = host
                    .call(move |actor| {
                        Box::pin(async move {
                            actor.finish_work(&session_id, &operation.run, result).await
                        })
                    })
                    .await;
            });
            Ok(())
        })
    }

    async fn finish_work(
        &mut self,
        session_id: &str,
        run_id: &str,
        result: Result<String>,
    ) -> Result<()> {
        let Some(work) = self.work.get_mut(session_id) else {
            return Ok(());
        };
        if !work
            .active
            .as_ref()
            .is_some_and(|active| active.id == run_id)
        {
            return Ok(());
        }
        let run = work
            .session
            .runs
            .iter_mut()
            .find(|run| run.id == run_id)
            .context("run missing")?;
        match result {
            Ok(report) => {
                run.outcome = w::Outcome::Completed.into();
                run.report = report;
            }
            Err(error) => {
                run.outcome = if error
                    .downcast_ref::<crate::error::ApiError>()
                    .is_some_and(|error| error.0 == connectrpc::ErrorCode::Unknown)
                {
                    w::Outcome::Unknown
                } else {
                    w::Outcome::Failed
                }
                .into();
                run.report = error
                    .downcast_ref::<crate::error::ApiError>()
                    .map_or(
                        "作業を完了できませんでした。外部操作の結果を確認してください。",
                        |error| error.1,
                    )
                    .into();
            }
        }
        let companion = work.session.companion_id.clone();
        let report = run.report.clone();
        work.active = None;
        let mut db = self.db.acquire().await?;
        store::put(&mut db, "work", session_id, work.revision, &work.session).await?;
        work.revision += 1;
        let session = work.session.clone();
        drop(db);
        self.approvals
            .retain(|_, pending| pending.card.run_id != run_id);
        self.emit(Payload::Work(Box::new(session)));
        self.report(companion, format!("[Work outcome] {report}"));
        self.start_work(session_id).await?;
        Ok(())
    }

    async fn stop_work(&mut self, request: w::StopRequest) -> Result<w::Empty> {
        let mutation = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("stop_work", &request))?;
        let mut db = self.db.acquire().await?;
        if let Some(saved) = store::replay(&mut db, &mutation.request_key, &hash).await? {
            return Ok(saved);
        }
        self.stop_session(&request.id, None).await?;
        let reply = w::Empty::default();
        store::acknowledge(&mut db, &mutation.request_key, &hash, &reply).await?;
        Ok(reply)
    }

    async fn stop_session(&mut self, session: &str, scope: Option<&str>) -> Result<()> {
        let Some(work) = self.work.get(session) else {
            return Ok(());
        };
        if scope.is_some_and(|scope| scope != work.session.companion_id) {
            return Err(denied("work belongs to another companion"));
        }
        let Some(mut work) = self.work.remove(session) else {
            return Ok(());
        };
        let mut db = self.db.acquire().await?;
        if let Some(active) = work.active.take() {
            active.cancel.cancel();
            self.approvals
                .retain(|_, pending| pending.card.run_id != active.id);
        }
        for run in &mut work.session.runs {
            if matches!(
                run.outcome.as_known(),
                Some(w::Outcome::Running | w::Outcome::Unspecified)
            ) {
                run.outcome = w::Outcome::Stopped.into();
                run.report = "作業を中断しました。実行済みの変更は元に戻りません。".into();
            }
        }
        store::put(&mut db, "work", session, work.revision, &work.session).await?;
        self.emit(Payload::Work(Box::new(work.session)));
        self.announce_snapshot().await?;
        Ok(())
    }

    fn tick_work(&mut self) {
        let mut notices = Vec::new();
        for work in self.work.values_mut() {
            if work.active.is_some()
                && !work.warned
                && work.last_activity.elapsed() >= Duration::from_secs(120)
            {
                work.warned = true;
                notices.push(w::Notice {
                    companion_id: work.session.companion_id.clone(),
                    text: "作業からしばらく報告がありません。".into(),
                    ..Default::default()
                });
            }
        }
        for notice in notices {
            self.emit(Payload::Notice(Box::new(notice)));
        }
    }
}
