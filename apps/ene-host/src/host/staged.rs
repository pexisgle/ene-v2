//! Heartbeat ticks: one direct model call, promoted only when it speaks
//! (docs/features/autonomy/schedule.md §4, docs/design/tech-stack.md §B).
//!
//! A tick must never leave a silent turn in the companion's append-only session
//! log, so it is not a kernel turn. The request is built from the companion's
//! saved `Conversation` the same way compaction builds its summary request: the
//! `ene.summary` snapshot is injected as the head block, the heartbeat prompt is
//! the last user message, and the dynamic tail an ordinary turn carries (clock,
//! mind, pinned memories, automatic recall, perception) is attached to it. The
//! reply is read, never continued, so nothing is executed; only text that says
//! something is spliced into the main `Conversation` as the tick's input and the
//! assistant's answer. A silent, `noop` or tool-calling reply writes nothing, so
//! the DirStore never sees a heartbeat that decided to stay quiet.
//!
//! The call runs on the host's Tokio runtime, one tick at a time; a user message
//! or the timeout aborts the task and drops the request with it, so no interrupt
//! leaves a tick running.
use super::{
    agent::{self, CompanionAgent},
    background::Background,
    compaction::{self, InactivityTracker, Weigher},
    db::{Result, Store},
    error::HostError,
    mind::{
        Companion, Role,
        embedding::CompactVectorIndex,
        recall::{self, Recall},
    },
    model::{self, Bound, Credentials},
    resources::{HostRuntime, HostState, Perception},
};
use bevy_ecs::prelude::*;
use rig_core::completion::{
    AssistantContent, AssistantMessage, CompletionRequest, CompletionResponse, Message,
    ToolDefinition,
};
use rig_core::conversation::{Author, Conversation as History, Entry};
use rig_core::serve::ErasedHandler;
use rig_ecs::prelude::*;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// Ticks need this much user idle time, like the compaction soft condition.
pub(crate) const IDLE: Duration = Duration::from_secs(60);
/// A tick that has not settled by then is discarded and its call cancelled.
const TIMEOUT: Duration = Duration::from_secs(120);
/// The instruction the head gets for a heartbeat tick. It decides between
/// speaking and `noop`, exactly as for a schedule firing.
const TICK: &str =
    "定期Tick: ユーザーはしばらく無操作です。今話しかけるべきか、noop か判断してください";

/// The one tick in flight, and when the last one started so ticks keep the
/// idle cadence instead of running on every frame an idle user leaves.
#[derive(Resource, Default)]
pub(crate) struct StagedTick {
    in_flight: Option<InFlight>,
    last_started: Option<Instant>,
    /// The companion to try first next time; failed starts do not starve later ones.
    next: usize,
}

struct InFlight {
    companion: String,
    /// The heartbeat prompt, exactly the user entry a promotion writes.
    prompt: String,
    started: Instant,
    /// The main conversation length when the tick started; a later turn
    /// must not be spliced into.
    messages: usize,
    /// The running call; aborting it drops the request in flight.
    call: Background<Outcome>,
}

/// What the tick's call produced: an answer to promote, silence, or a failure
/// that is only logged.
type Outcome = std::result::Result<Option<AssistantMessage>, String>;

impl StagedTick {
    /// Aborts the tick in flight, if any.
    fn cancel(&mut self) {
        if let Some(in_flight) = self.in_flight.take() {
            in_flight.call.abort();
        }
    }
}

impl Drop for StagedTick {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Starts a tick when the user is idle and the companion can answer right now:
/// the request is built here, the call runs on the host runtime.
#[allow(clippy::too_many_arguments)]
fn begin_tick(
    mut ticks: ResMut<StagedTick>,
    tracker: Option<Res<InactivityTracker>>,
    host: Option<Res<HostState>>,
    store: Option<Res<Store>>,
    credentials: Option<Res<Credentials>>,
    models: Option<Res<Models>>,
    runtime: Option<Res<HostRuntime>>,
    perception: Option<Res<Perception>>,
    index: Option<ResMut<CompactVectorIndex>>,
    tools: Query<&ToolDef>,
    agents: Query<(&CompanionAgent, &Conversation, Has<ActiveTurn>)>,
    wake: Option<Res<Wake>>,
    #[cfg(test)] tick_model: Option<Res<agent::TestModel>>,
) {
    if ticks.in_flight.is_some()
        || ticks.last_started.is_some_and(|last| last.elapsed() < IDLE)
        || host
            .as_ref()
            .is_some_and(|host| host.failed.is_some() || host.closing)
    {
        return;
    }
    let Some(tracker) = tracker else {
        return;
    };
    if tracker.idle_duration() < IDLE {
        return;
    }
    let (Some(store), Some(perception), Some(runtime), Some(mut index)) =
        (store, perception, runtime, index)
    else {
        return;
    };
    let companions = store.companions().unwrap_or_default();
    let count = companions.len();
    for attempt in 0..count {
        let position = (ticks.next + attempt) % count;
        let companion = &companions[position];
        let Some((conversation, busy)) = agents
            .iter()
            .find(|(agent, ..)| agent.companion_id == companion.id)
            .map(|(_, conversation, busy)| (conversation, busy))
        else {
            continue;
        };
        if busy {
            continue;
        }
        // In a test, an injected mock handler answers instead of a provider.
        #[cfg(test)]
        let bound = tick_model.as_deref().map_or_else(
            || connect(&store, credentials.as_deref(), models.as_deref(), companion),
            |model| Ok((model.0)()),
        );
        #[cfg(not(test))]
        let bound = connect(&store, credentials.as_deref(), models.as_deref(), companion);
        let Ok(bound) = bound else {
            continue;
        };
        // The recall query is the saved conversation's newest user text, as
        // the ordinary turn's tail uses it, before the summary stands in for
        // the messages it replaces.
        let messages = conversation.0.messages().len();
        let query = recall::recall_query(&conversation.0);
        let mut history = conversation.0.view();
        if let Some(summary) = compaction::summary_of(&history) {
            compaction::inject_summary(&mut history, &summary);
        }
        let tail = recall::dynamic_tail(
            &store,
            &perception,
            &companion.id,
            query.as_deref(),
            Recall {
                index: &mut index,
                runtime: Some(&runtime.0),
                credentials: credentials.as_deref(),
            },
            None,
        );
        history.push(recall::with_tail(&Message::user(TICK), &tail));
        let Some(request) = request(companion, &mut history, &bound, &tools) else {
            continue;
        };
        let handler = bound.connection.handler.0.clone();
        let call = Background::spawn(&runtime.0, wake.as_deref(), async move {
            tokio::time::timeout(TIMEOUT, ask(request, handler))
                .await
                .unwrap_or_else(|_| Err("the heartbeat tick timed out".to_owned()))
        });
        let started = Instant::now();
        ticks.last_started = Some(started);
        ticks.next = (position + 1) % count;
        ticks.in_flight = Some(InFlight {
            companion: companion.id.clone(),
            prompt: TICK.to_owned(),
            messages,
            started,
            call,
        });
        return;
    }
    ticks.last_started = Some(Instant::now());
}

/// The companion model a tick calls.
fn connect(
    store: &Store,
    credentials: Option<&Credentials>,
    models: Option<&Models>,
    companion: &Companion,
) -> Result<Bound> {
    let (Some(credentials), Some(models)) = (credentials, models) else {
        return Err(HostError::unavailable(
            "the heartbeat tick has no model connection",
        ));
    };
    model::connect(
        store,
        credentials,
        models.0.catalog(),
        companion,
        Role::Companion,
    )
}

/// The request the tick sends: the built history (summary head, conversation,
/// the heartbeat prompt with its tail), the companion's system prompt and the
/// `noop` tool so it can choose silence, fitted to the model's window.
fn request(
    companion: &Companion,
    history: &mut History,
    bound: &Bound,
    tools: &Query<&ToolDef>,
) -> Option<CompletionRequest> {
    let spec = Arc::clone(&bound.connection.spec);
    let prompt = agent::system_prompt(companion);
    let noop: Vec<Arc<ToolDefinition>> = tools
        .iter()
        .filter(|tool| tool.0.name.as_str() == "noop")
        .map(|tool| Arc::clone(&tool.0))
        .collect();
    let mut request = CompletionRequest::from(Vec::<Message>::new())
        .tools(noop)
        .options(spec.default_options(None));
    let _ = spec.fit(&mut request);
    let reference = spec.reference();
    let limit = spec.input_limit().map(u64::from).unwrap_or(128_000);
    let reserve = u64::from(spec.max_output_tokens.unwrap_or(4_096)) + limit / 10;
    let weight = Weigher {
        model: &reference,
        prompt: Some(&prompt),
        tools: &request.tools,
        anchor: None,
        sent: None,
    };
    if !compaction::fit(history, &weight, limit, reserve) {
        tracing::warn!(companion = %companion.id, "the heartbeat tick does not fit the model window");
        return None;
    }
    request.chat_history = history.messages().to_vec();
    Some(request.preamble(prompt))
}

/// Asks the head once, outside the kernel: the reply is read, not continued,
/// so a tool call has run nothing.
async fn ask(request: CompletionRequest, handler: ErasedHandler) -> Outcome {
    model::unary(&handler, request)
        .await
        .map(|response| acting_reply(&response))
}

/// The reply a promotion writes: the text blocks, when the head spoke. A reply
/// that calls a tool is no action — nothing ran, and `noop` is silence.
fn acting_reply(response: &CompletionResponse) -> Option<AssistantMessage> {
    if response
        .choice
        .iter()
        .any(|content| matches!(content, AssistantContent::ToolCall(_)))
    {
        return None;
    }
    let content: Vec<AssistantContent> = response
        .choice
        .iter()
        .filter(|content| matches!(content, AssistantContent::Text(_)))
        .cloned()
        .collect();
    let text: String = content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    (!text.trim().is_empty()).then(|| AssistantMessage::new(content))
}

/// Settles the tick in flight: a user message or the timeout aborts it, an
/// acting reply is promoted, and a silent or failed one is discarded.
fn settle_tick(
    mut ticks: ResMut<StagedTick>,
    tracker: Option<Res<InactivityTracker>>,
    mut agents: Query<(&CompanionAgent, &mut Conversation, Option<&ActiveTurn>)>,
) {
    let Some(in_flight) = ticks.in_flight.as_ref() else {
        return;
    };
    let interrupted = tracker
        .as_deref()
        .is_some_and(|tracker| tracker.last_user_interaction_at >= in_flight.started);
    if interrupted || in_flight.started.elapsed() > TIMEOUT {
        ticks.cancel();
        return;
    }
    if !in_flight.call.is_finished() {
        return;
    }
    let Some(in_flight) = ticks.in_flight.take() else {
        return;
    };
    let outcome = in_flight
        .call
        .take()
        .unwrap_or_else(|| Err("the heartbeat tick was cancelled".to_owned()));
    match outcome {
        Ok(Some(reply)) => {
            // A turn that began while the tick was in flight owns the tail;
            // another conversation change also invalidates the splice.
            let safe = agents
                .iter_mut()
                .find(|(agent, ..)| agent.companion_id == in_flight.companion)
                .map(|(_, conversation, busy)| {
                    busy.is_none() && conversation.messages().len() == in_flight.messages
                })
                .unwrap_or(false);
            if safe {
                promote(&mut agents, &in_flight.companion, &in_flight.prompt, reply);
            }
        }
        Ok(None) => {}
        Err(why) => {
            tracing::warn!(%why, companion = %in_flight.companion, "the heartbeat tick failed")
        }
    }
}

/// Writes an acting tick into the companion's conversation as one splice: the
/// heartbeat input and the reply, in that order.
fn promote(
    agents: &mut Query<(&CompanionAgent, &mut Conversation, Option<&ActiveTurn>)>,
    companion: &str,
    prompt: &str,
    reply: AssistantMessage,
) {
    let Some((_, mut conversation, _)) = agents
        .iter_mut()
        .find(|(agent, ..)| agent.companion_id == companion)
    else {
        return;
    };
    let at = conversation.messages().len();
    let by = Author::plugin("ene-host");
    conversation.splice(
        at..,
        [
            Entry::new(Message::user(prompt)).by(by.clone()),
            Entry::new(Message::Assistant(reply)),
        ],
        by,
    );
}

/// Registers the heartbeat tick: read the companion, call the model once,
/// promote a reply that speaks.
pub(crate) struct StagedPlugin;

impl Plugin for StagedPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<StagedTick>()
            .add_systems(Update, (begin_tick, settle_tick).chain().after(AgentLoop));
    }
}

/// Test seam: the handler a tick calls, so no test builds a provider
#[cfg(test)]
mod tests;
