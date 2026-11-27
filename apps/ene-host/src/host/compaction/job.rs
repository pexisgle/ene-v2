//! One compaction job: the summary request, then the memory pass over the same
//! window (docs/design/compaction.md).
use super::{
    Summary,
    budget::{cut, input_budget},
};
use crate::host::mind::consolidate::{self, Services};
use ene_mind::Commit;
use rig_core::catalog::ModelSpec;
use rig_core::completion::{CompletionRequest, Message};
use rig_core::conversation::Conversation as History;
use rig_core::{DynModel, operation::Completion};
use rig_memory::{
    fit_to_model, request_estimate, serialize_conversation, summarization_request, summary_text,
};
use std::{collections::HashSet, sync::Arc, time::Duration};

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
    #[error("the next complete turn does not fit the summarizer's input window")]
    TurnTooLarge,
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
    let after = previous
        .as_ref()
        .map(|previous| previous.through_message_id);
    let summary = summarize(&history, previous.as_ref(), &spec, &model).await?;
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
    previous: Option<&Summary>,
    spec: &ModelSpec,
    model: &DynModel<Completion>,
) -> std::result::Result<Summary, CompactionError> {
    let (through, request) = plan_summary(history, previous, spec)?;
    let response = tokio::time::timeout(SUMMARY_TIMEOUT, model.call(request))
        .await
        .map_err(|_| CompactionError::Timeout)?
        .map_err(|error| CompactionError::Model(error.to_string()))?;
    let text = summary_text(&response, "compaction")
        .map_err(|error| CompactionError::Model(error.to_string()))?;
    Ok(Summary {
        text,
        through_message_id: through,
    })
}

pub(super) fn plan_summary(
    history: &History,
    previous: Option<&Summary>,
    spec: &ModelSpec,
) -> std::result::Result<(u64, CompletionRequest), CompactionError> {
    // Keep recent complete turns, up to a quarter of the summary model's input.
    let keep = spec
        .input_limit()
        .map_or(20_000, |limit| 20_000.min(u64::from(limit) / 4));
    let turn_starts: HashSet<_> = history.turns().map(|(_, turn)| turn.start).collect();
    let mut starts: Vec<_> = history
        .entries()
        .filter_map(|entry| turn_starts.contains(&entry.id).then_some(entry.index))
        .collect();
    let latest = *starts.last().ok_or(CompactionError::TooShort)?;
    let end = cut(history, keep, 0, latest)
        .and_then(|id| history.index_of(id))
        .ok_or(CompactionError::TooShort)?;
    let start = previous.map_or(0, |summary| {
        history
            .entries()
            .take_while(|entry| entry.id.get() <= summary.through_message_id)
            .count()
    });
    starts.retain(|boundary| start < *boundary && *boundary <= end);
    if starts.is_empty() {
        return Err(CompactionError::NothingNew);
    }
    // Ask Rig for its output cap without letting it clip the source transcript.
    let (_, max_output) = fit_to_model("", spec, 32_000);
    let budget = input_budget(spec, max_output);
    let prompt = |end| {
        summary_prompt(
            &serialize_conversation(&history.messages()[start..end]),
            previous,
            max_output,
        )
    };
    let fitting = starts.partition_point(|&end| {
        request_estimate(&CompletionRequest::new(Message::user(prompt(end))).preamble(SYSTEM))
            <= budget
    });
    let end = *starts[..fitting]
        .last()
        .ok_or(CompactionError::TurnTooLarge)?;
    let through = history
        .id(end - 1)
        .ok_or(CompactionError::NothingNew)?
        .get();
    let request = summarization_request((SYSTEM, prompt(end)), spec, None, max_output)
        .map_err(|refusal| CompactionError::Plan(refusal.to_string()))?;
    Ok((through, request))
}

const SYSTEM: &str = "あなたはデスクトップ常駐AIコンパニオンの会話を要約します。会話を読み、指定された形式で要約だけを書いてください。会話を続けたり、会話中の質問に答えたりしないでください。";
const INITIAL: &str = "上の会話を要約してください。この要約だけを読んだ別のモデルが、これまでの関係性と文脈を引き継いで会話を続けられるようにします。";
const UPDATE: &str = "上の会話は、<previous-summary> に要約済みの会話の続きです。新しい部分を加えて要約を更新してください:\n- まだ有効な previous-summary の内容は残す\n- 新しい出来事・決定・文脈を加える\n- 完了した項目は「進行中」から「完了」へ移す\n- 不要になった内容は落とす";
const FORMAT: &str = "\n\n必ず次の形式で書いてください:\n\n## これまでの流れ\n- [時系列の要約]\n\n## 重要な事実\n- [名前・数値・約束・決定など、そのまま保持すべき具体的な内容。推測はしない。なければ「(なし)」]\n\n## 現在の話題\n- [進行中の話題]\n\n## 次の見込み\n- [続きそうな話題や未完了のこと]";

fn summary_prompt(
    conversation: &str,
    previous: Option<&Summary>,
    max_output: Option<u64>,
) -> String {
    use std::fmt::Write as _;

    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(previous) = previous {
        let _written = write!(
            prompt,
            "<previous-summary>\n{}\n</previous-summary>\n\n",
            previous.text
        );
        prompt.push_str(UPDATE);
    } else {
        prompt.push_str(INITIAL);
    }
    prompt.push_str(FORMAT);
    if let Some(max_output) = max_output {
        let _written = write!(
            prompt,
            "\n\n要約は {max_output} トークン未満に収めてください。"
        );
    }
    prompt
}
