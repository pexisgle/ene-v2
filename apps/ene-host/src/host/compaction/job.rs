//! One compaction job: the summary request, then the memory pass over the same
//! window (docs/design/mind.md).
use super::{
    Summary,
    budget::{cut, input_budget},
    log_lineage, saved_summary,
};
use crate::host::mind::consolidate::{self, Services};
use ene_mind::Checkpoint;
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
    #[error("the summarizer's window does not fit the instructions and any source text")]
    InputWindow,
    #[error("the summarizer timed out")]
    Timeout,
    #[error("{0}")]
    Model(String),
    #[error("the consolidation pass left its range pending: {0}")]
    Consolidation(#[from] crate::host::error::HostError),
}

pub(super) type Outcome = std::result::Result<Summary, CompactionError>;

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
    let lineage = log_lineage(&history).map_err(crate::host::error::HostError::from)?;
    if let Some(saved) = saved_summary(&services.store, &companion, &history)
        .map_err(crate::host::error::HostError::from)?
        && previous.as_ref() != Some(&saved)
    {
        return Ok(saved);
    }
    let summary = summarize(&history, previous.as_ref(), &spec, &model).await?;
    Ok(consolidate::pass(
        &services,
        &companion,
        &history,
        previous.map(|summary| summary.through_message_id),
        Checkpoint { lineage, summary },
    )
    .await?)
}

/// Asks for the summary on the shared background runtime.
pub(super) async fn summarize(
    history: &History,
    previous: Option<&Summary>,
    spec: &ModelSpec,
    model: &DynModel<Completion>,
) -> std::result::Result<Summary, CompactionError> {
    let (through, input) = plan_summary(history, previous, spec)?;
    let text = tokio::time::timeout(SUMMARY_TIMEOUT, async {
        match input {
            SummaryInput::Single(request) => ask_summary(model, *request).await,
            SummaryInput::Segments {
                transcript,
                max_output,
            } => summarize_segments(model, spec, previous, &transcript, max_output).await,
        }
    })
    .await
    .map_err(|_| CompactionError::Timeout)??;
    Ok(Summary {
        text,
        through_message_id: through,
    })
}

pub(super) enum SummaryInput {
    Single(Box<CompletionRequest>),
    Segments {
        transcript: String,
        max_output: Option<u64>,
    },
}

pub(super) fn plan_summary(
    history: &History,
    previous: Option<&Summary>,
    spec: &ModelSpec,
) -> std::result::Result<(u64, SummaryInput), CompactionError> {
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
    let transcript = |end| serialize_conversation(&history.messages()[start..end]);
    let prior = previous.map(|summary| summary.text.as_str());
    let fitting = starts.partition_point(|&end| {
        prompt_fits(&summary_prompt(&transcript(end), prior, max_output), budget)
    });
    let end = starts[fitting.saturating_sub(1)];
    let through = history
        .id(end - 1)
        .ok_or(CompactionError::NothingNew)?
        .get();
    let transcript = transcript(end);
    let input = if fitting == 0 {
        SummaryInput::Segments {
            transcript,
            max_output,
        }
    } else {
        SummaryInput::Single(Box::new(summary_request(
            spec,
            &transcript,
            prior,
            max_output,
        )?))
    };
    Ok((through, input))
}

fn summary_request(
    spec: &ModelSpec,
    transcript: &str,
    previous: Option<&str>,
    max_output: Option<u64>,
) -> Result<CompletionRequest, CompactionError> {
    let prompt = summary_prompt(transcript, previous, max_output);
    summarization_request((SYSTEM, prompt), spec, None, max_output)
        .map_err(|refusal| CompactionError::Plan(refusal.to_string()))
}

async fn ask_summary(
    model: &DynModel<Completion>,
    request: CompletionRequest,
) -> Result<String, CompactionError> {
    let response = model
        .call(request)
        .await
        .map_err(|error| CompactionError::Model(error.to_string()))?;
    summary_text(&response, "compaction").map_err(|error| CompactionError::Model(error.to_string()))
}

async fn summarize_segments(
    model: &DynModel<Completion>,
    spec: &ModelSpec,
    previous: Option<&Summary>,
    transcript: &str,
    max_output: Option<u64>,
) -> Result<String, CompactionError> {
    let budget = input_budget(spec, max_output);
    let mut summary = previous.map(|previous| previous.text.clone());
    if let Some(old) = &summary
        && segment_end(transcript, Some(old), max_output, budget).is_none()
    {
        summary = Some(roll_summary(model, spec, old, None, max_output, budget).await?);
    }
    roll_summary(model, spec, transcript, summary, max_output, budget).await
}

async fn roll_summary(
    model: &DynModel<Completion>,
    spec: &ModelSpec,
    mut remaining: &str,
    mut previous: Option<String>,
    max_output: Option<u64>,
    budget: u64,
) -> Result<String, CompactionError> {
    while !remaining.is_empty() {
        let end = segment_end(remaining, previous.as_deref(), max_output, budget)
            .ok_or(CompactionError::InputWindow)?;
        let (segment, rest) = remaining.split_at(end);
        let request = summary_request(spec, segment, previous.as_deref(), max_output)?;
        previous = Some(ask_summary(model, request).await?);
        remaining = rest;
    }
    previous.ok_or(CompactionError::NothingNew)
}

fn segment_end(
    text: &str,
    previous: Option<&str>,
    max_output: Option<u64>,
    budget: u64,
) -> Option<usize> {
    // Rig's token heuristic charges at least one token per four UTF-8 bytes.
    let max = usize::try_from(budget.saturating_mul(4)).unwrap_or(usize::MAX);
    let max = text.floor_char_boundary(max.min(text.len()));
    let boundaries: Vec<_> = text[..max]
        .char_indices()
        .skip(1)
        .map(|(byte, _)| byte)
        .chain((max > 0).then_some(max))
        .collect();
    let fitting = boundaries.partition_point(|&end| {
        prompt_fits(&summary_prompt(&text[..end], previous, max_output), budget)
    });
    boundaries[..fitting].last().copied()
}

fn prompt_fits(prompt: &str, budget: u64) -> bool {
    request_estimate(&CompletionRequest::new(Message::user(prompt)).preamble(SYSTEM)) <= budget
}

const SYSTEM: &str = "あなたはデスクトップ常駐AIコンパニオンの会話を要約します。会話を読み、指定された形式で要約だけを書いてください。会話を続けたり、会話中の質問に答えたりしないでください。";
const INITIAL: &str = "上の会話を要約してください。この要約だけを読んだ別のモデルが、これまでの関係性と文脈を引き継いで会話を続けられるようにします。";
const UPDATE: &str = "上の会話は、<previous-summary> に要約済みの会話の続きです。新しい部分を加えて要約を更新してください:\n- まだ有効な previous-summary の内容は残す\n- 新しい出来事・決定・文脈を加える\n- 完了した項目は「進行中」から「完了」へ移す\n- 不要になった内容は落とす";
const FORMAT: &str = "\n\n必ず次の形式で書いてください:\n\n## これまでの流れ\n- [時系列の要約]\n\n## 重要な事実\n- [名前・数値・約束・決定など、そのまま保持すべき具体的な内容。推測はしない。なければ「(なし)」]\n\n## 現在の話題\n- [進行中の話題]\n\n## 次の見込み\n- [続きそうな話題や未完了のこと]";

fn summary_prompt(conversation: &str, previous: Option<&str>, max_output: Option<u64>) -> String {
    use std::fmt::Write as _;

    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(previous) = previous {
        let _written = write!(
            prompt,
            "<previous-summary>\n{}\n</previous-summary>\n\n",
            previous
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
