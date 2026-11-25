//! One compaction job: the summary request, then the memory pass over the same
//! window (docs/design/compaction.md).
use super::Summary;
use crate::host::mind::consolidate::{self, Commit, Services};
use rig_core::catalog::ModelSpec;
use rig_core::conversation::{Conversation as History, MessageId};
use rig_core::{DynModel, operation::Completion};
use rig_memory::{CompactReason, CompactionPolicy, Summarizer, SummaryState};
use std::{sync::Arc, time::Duration};

/// A provider that never answers must not block every companion's summaries.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub(super) enum CompactionError {
    #[error("the summary task ended without a result")]
    TaskMissingResult,
    #[error("{0}")]
    Plan(String),
    #[error("the conversation is too short to summarize")]
    TooShort,
    #[error("there is nothing new to summarize")]
    NothingNew,
    #[error("the summarizer timed out")]
    Timeout,
    #[error("{0}")]
    Model(String),
}

pub(super) type Outcome = std::result::Result<Collected, CompactionError>;

/// What one job produced: the summary the aside keeps, and the memories the
/// pass committed for the same window when it succeeded.
pub(super) struct Collected {
    pub(super) summary: Summary,
    pub(super) memories: crate::host::db::Result<Commit>,
}

/// One job: the summary, then the memory pass over the window that summary
/// stands for. The summary runs first because its `through_message_id` is the
/// window's end, so a failed summary commits nothing.
pub(super) async fn run(
    history: History,
    previous: Option<Summary>,
    spec: Arc<ModelSpec>,
    model: DynModel<Completion>,
    services: Services,
    companion: String,
) -> Outcome {
    let (text, after) = previous.map_or_else(
        || (String::new(), None),
        |previous| (previous.text, Some(previous.through_message_id)),
    );
    let summary = summarize(&history, text, spec, model).await?;
    let memories = consolidate::pass(
        &services,
        &companion,
        &history,
        after,
        summary.through_message_id,
    )
    .await;
    Ok(Collected { summary, memories })
}

/// Asks for the summary on the shared background runtime.
async fn summarize(
    history: &History,
    previous: String,
    spec: Arc<ModelSpec>,
    model: DynModel<Completion>,
) -> std::result::Result<Summary, CompactionError> {
    let state = SummaryState {
        summary: previous,
        tracked: Vec::new(),
    };
    let plan = policy()
        .plan_summary(history, &state, &CompactReason::Threshold, &spec)
        .map_err(|refusal| CompactionError::Plan(refusal.to_string()))?
        .ok_or(CompactionError::TooShort)?;
    let through = through_message_id(history, plan.keep_from).ok_or(CompactionError::NothingNew)?;
    let response = tokio::time::timeout(SUMMARY_TIMEOUT, model.call(plan.request))
        .await
        .map_err(|_| CompactionError::Timeout)?
        .map_err(|error| CompactionError::Model(error.to_string()))?;
    let text = Summarizer::summary_text(&response)
        .map_err(|error| CompactionError::Model(error.to_string()))?;
    Ok(Summary {
        text,
        through_message_id: through,
    })
}

/// The id of the newest message a summary ending before `keep_from` stands for.
fn through_message_id(history: &History, keep_from: Option<MessageId>) -> Option<u64> {
    let at = match keep_from {
        Some(id) => history.index_of(id)?,
        None => history.messages().len(),
    };
    history
        .id(at.checked_sub(1)?)
        .map(rig_core::conversation::MessageId::get)
}

/// The summarizer policy: prompts written for a companion conversation.
fn policy() -> CompactionPolicy {
    let mut policy = CompactionPolicy::default();
    let prompts = &mut policy.summarizer.prompts;
    prompts.system = "あなたはデスクトップ常駐AIコンパニオンの会話を要約します。会話を読み、指定された形式で要約だけを書いてください。会話を続けたり、会話中の質問に答えたりしないでください。".into();
    prompts.initial = "上の会話を要約してください。この要約だけを読んだ別のモデルが、これまでの関係性と文脈を引き継いで会話を続けられるようにします。".into();
    prompts.update = "上の会話は、<previous-summary> に要約済みの会話の続きです。新しい部分を加えて要約を更新してください:\n- まだ有効な previous-summary の内容は残す\n- 新しい出来事・決定・文脈を加える\n- 完了した項目は「進行中」から「完了」へ移す\n- 不要になった内容は落とす".into();
    prompts.format = "\n\n必ず次の形式で書いてください:\n\n## これまでの流れ\n- [時系列の要約]\n\n## 重要な事実\n- [名前・数値・約束・決定など、そのまま保持すべき具体的な内容。推測はしない。なければ「(なし)」]\n\n## 現在の話題\n- [進行中の話題]\n\n## 次の見込み\n- [続きそうな話題や未完了のこと]".into();
    policy
}
