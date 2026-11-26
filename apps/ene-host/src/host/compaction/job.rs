//! One compaction job: the summary request, then the memory pass over the same
//! window (docs/design/compaction.md).
use super::{Summary, budget::cut};
use crate::host::mind::consolidate::{self, Services};
use ene_mind::Commit;
use rig_core::catalog::ModelSpec;
use rig_core::conversation::Conversation as History;
use rig_core::{DynModel, operation::Completion};
use rig_memory::{fit_to_model, serialize_conversation, summarization_request, summary_text};
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
    // Keep recent complete turns, up to a quarter of the summary model's input.
    let keep = spec
        .input_limit()
        .map_or(20_000, |limit| 20_000.min(u64::from(limit) / 4));
    let end = cut(history, keep, 0)
        .and_then(|id| history.index_of(id))
        .ok_or(CompactionError::TooShort)?;
    let through = history
        .id(end - 1)
        .ok_or(CompactionError::NothingNew)?
        .get();
    let start = previous.map_or(0, |summary| {
        history
            .entries()
            .take_while(|entry| entry.id.get() <= summary.through_message_id)
            .count()
    });
    let messages = history
        .messages()
        .get(start..end)
        .filter(|messages| !messages.is_empty())
        .ok_or(CompactionError::NothingNew)?;
    let conversation = serialize_conversation(messages);
    let (conversation, max_output) = fit_to_model(&conversation, spec, 32_000);
    let prompt = summary_prompt(&conversation, previous, max_output);
    let request = summarization_request((SYSTEM, prompt), spec, None, max_output)
        .map_err(|refusal| CompactionError::Plan(refusal.to_string()))?;
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
