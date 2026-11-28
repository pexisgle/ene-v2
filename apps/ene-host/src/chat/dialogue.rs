use crate::schedule::ScheduleCommands as _;

use super::*;

impl Actor {
    pub(super) async fn start_dialogue(
        &mut self,
        companion: w::Companion,
        turn: String,
        input: String,
        trigger: Trigger,
    ) -> Result<()> {
        self.cancel_turn(&companion.id);
        let connection = self
            .selection(&companion, model_selection::Role::Dialogue)
            .await;
        let (selection, connection) = match connection {
            Ok(value) => value,
            Err(_) if trigger == Trigger::Schedule => return Ok(()),
            Err(_) => {
                self.emit(Payload::Notice(Box::new(w::Notice {
                    companion_id: companion.id.clone(),
                    text: "対話モデルを設定してください。".into(),
                    ..Default::default()
                })));
                self.turn_state(companion.id, turn, w::Outcome::Failed);
                return Ok(());
            }
        };
        let mut context = self.conversation(&companion, &turn, input).await?;
        if trigger == Trigger::Group {
            context.system.push_str("\n音声は他のコンパニオンにも届いています。自分宛てか、話題と自分の人格から判断してください。自分宛てでなければツールを使わず <silent/> とだけ答えてください。");
        }
        let embedding = self
            .selection(&companion, model_selection::Role::Embedding)
            .await
            .ok();
        let db = self.db.clone();
        let cancel = self.host.shutdown.child_token();
        self.turns.insert(
            companion.id.clone(),
            Turn {
                id: turn.clone(),
                cancel: cancel.clone(),
            },
        );
        self.turn_state(companion.id.clone(), turn.clone(), w::Outcome::Running);
        let host = self.host.clone();
        let models = self.models.clone();
        let dialogue = std::sync::Arc::new(Dialogue {
            companion: companion.id,
            turn,
            direct: matches!(trigger, Trigger::User | Trigger::Group)
                .then(|| context.input.clone()),
            cancel,
            grouped: trigger == Trigger::Group,
            scheduled: trigger == Trigger::Schedule,
            deferred: Default::default(),
        });
        self.jobs.spawn(async move {
            let result = async {
                if let Some((selection, connection)) = embedding {
                    let retrieval = async {
                        let model = models.model(selection, connection).await?;
                        models
                            .vectors
                            .recall(
                                &db,
                                &model,
                                crate::vectors::Recall {
                                    companion: &dialogue.companion,
                                    question: &context.input,
                                    memories: &context.memories,
                                },
                                &dialogue.cancel,
                            )
                            .await
                    };
                    if let Ok(Ok(memories)) =
                        tokio::time::timeout(std::time::Duration::from_secs(8), retrieval).await
                    {
                        let companion = dialogue.companion.clone();
                        let memories = host
                            .call(move |actor| {
                                Box::pin(async move {
                                    actor.visible_memories(&companion, memories).await
                                })
                            })
                            .await?;
                        context.memories = memories;
                        context.dynamic = dynamic(
                            &context.state,
                            &context.memories,
                            context.observation.as_ref(),
                        )?;
                    }
                }
                let model = models.model(selection, connection).await?;
                let tools = dialogue_tools(trigger)?;
                let (output, receive) = tokio::sync::mpsc::channel::<Option<String>>(32);
                let observed = observe_reply(&host, &dialogue, receive);
                let generation = model.respond(
                    context.prompt(),
                    tools,
                    &dialogue.cancel,
                    (!dialogue.scheduled).then_some(output),
                    |call| execute_tool(host.clone(), dialogue.clone(), call),
                );
                let (text, (speech, sentence)) = tokio::join!(generation, observed);
                let text = text?;
                if dialogue.deferred.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(invalid("scheduled work needs a configured work model"));
                }
                let streamed = speech.is_some();
                if let Some(queue) = speech
                    && !sentence.trim().is_empty()
                {
                    let _closed = queue.send(sentence).await;
                }
                Ok((text, streamed))
            }
            .await;
            let _closed = host
                .call(move |actor| {
                    Box::pin(async move {
                        actor
                            .finish_dialogue(&dialogue, context.memories, result)
                            .await
                    })
                })
                .await;
        });
        Ok(())
    }

    async fn conversation(
        &mut self,
        companion: &w::Companion,
        turn: &str,
        input: String,
    ) -> Result<Conversation> {
        let character = {
            let mut db = self.db.acquire().await?;
            store::get::<w::Character>(&mut db, "character", &companion.character_id)
                .await?
                .ok_or_else(|| missing("character not found"))?
        };
        let mut mind = self.mind(&companion.id).await?;
        if now().saturating_sub(mind.observed_at) >= 21600 {
            mind.emotion.clear();
        }
        let memories = self.recall(&companion.id, &input).await?;
        let observation = self
            .observations
            .get(&companion.id)
            .and_then(|current| current.value.clone());
        let dynamic = dynamic(&mind, &memories, observation.as_ref())?;
        self.restore_summary(&companion.id).await?;
        let journal = self.journal(&companion.id).await?;
        let summary = journal
            .summary
            .as_ref()
            .map_or("", |summary| summary.text.as_str());
        let start = journal
            .summary
            .as_ref()
            .map_or(0, |summary| summary.through as usize);
        let history = &journal.entries[start..];
        let history = if history.last().is_some_and(|last| last.turn_id == turn) {
            &history[..history.len() - 1]
        } else {
            history
        };
        Ok(Conversation {
            system: format!(
                "{SYSTEM}\nコンパニオン名: {}\n人格設定: {}",
                companion.name,
                serde_json::to_string(&character.profile)?
            ),
            summary: summary.into(),
            history: history.to_vec(),
            dynamic,
            state: mind,
            observation,
            input,
            memories,
        })
    }

    async fn finish_dialogue(
        &mut self,
        dialogue: &Dialogue,
        memories: Vec<w::Memory>,
        result: Result<(String, bool)>,
    ) -> Result<()> {
        if dialogue.cancel.is_cancelled()
            || !self
                .turns
                .get(&dialogue.companion)
                .is_some_and(|turn| turn.id == dialogue.turn)
        {
            return Ok(());
        }
        let result = match result {
            Ok((text, streamed)) => {
                self.persist_dialogue(dialogue, memories, text, streamed)
                    .await
            }
            Err(error) => Err(error),
        };
        let outcome = match result {
            Ok(()) => w::Outcome::Completed,
            Err(error) => {
                let message = error
                    .downcast_ref::<crate::error::ApiError>()
                    .map_or("応答を生成できませんでした。", |error| {
                        error.1
                    });
                self.emit(Payload::Notice(Box::new(w::Notice {
                    companion_id: dialogue.companion.clone(),
                    text: message.into(),
                    ..Default::default()
                })));
                w::Outcome::Failed
            }
        };
        self.turns.remove(&dialogue.companion);
        self.clean_group(&dialogue.turn);
        self.turn_state(dialogue.companion.clone(), dialogue.turn.clone(), outcome);
        self.maybe_reflect(&dialogue.companion).await?;
        Ok(())
    }
    async fn persist_dialogue(
        &mut self,
        dialogue: &Dialogue,
        memories: Vec<w::Memory>,
        text: String,
        streamed: bool,
    ) -> Result<()> {
        if !memories.is_empty() {
            self.journal(&dialogue.companion)
                .await?
                .append(Record::Injection {
                    turn_id: dialogue.turn.clone(),
                    memories: memories
                        .into_iter()
                        .map(|memory| (memory.id, memory.revision))
                        .collect(),
                })
                .await?;
        }
        let reply = !text.trim().is_empty() && text.trim() != "<silent/>" && self.claim(dialogue);
        if reply {
            let companion = &dialogue.companion;
            let trigger_exists = dialogue.direct.is_none()
                && self
                    .journal(companion)
                    .await?
                    .entries
                    .iter()
                    .any(|message| {
                        message.turn_id == dialogue.turn && message.role == Role::Trigger
                    });
            if dialogue.direct.is_none() && !trigger_exists {
                self.append_message(
                    companion,
                    Message {
                        id: id(),
                        turn_id: dialogue.turn.clone(),
                        role: Role::Trigger,
                        text: "ホストからの通知".into(),
                        time: now(),
                    },
                    None,
                )
                .await?;
            }
            self.append_message(
                companion,
                Message {
                    id: id(),
                    turn_id: dialogue.turn.clone(),
                    role: Role::Assistant,
                    text: text.clone(),
                    time: now(),
                },
                None,
            )
            .await?;
        }
        if dialogue.scheduled {
            self.complete_schedule(&dialogue.turn).await?;
        }
        if reply && !streamed {
            self.speak_if_configured(&dialogue.companion, text).await?;
        }
        Ok(())
    }
}

async fn observe_reply(
    host: &Host,
    dialogue: &std::sync::Arc<Dialogue>,
    mut receive: tokio::sync::mpsc::Receiver<Option<String>>,
) -> (Option<tokio::sync::mpsc::Sender<String>>, String) {
    let mut combined = String::new();
    let mut speech = None::<tokio::sync::mpsc::Sender<String>>;
    let mut sentence = String::new();
    while let Some(delta) = receive.recv().await {
        let cleared = delta.is_none();
        if cleared {
            combined.clear();
            sentence.clear();
            speech.take();
        }
        let text = delta.unwrap_or_default();
        let delta = if dialogue.grouped && combined.len() < 9 {
            combined.push_str(&text);
            if !cleared && "<silent/>".starts_with(combined.trim()) {
                continue;
            }
            combined.clone()
        } else {
            text
        };
        sentence.push_str(&delta);
        let boundary = sentence
            .rfind(['。', '！', '？', '.', '!', '?', '\n'])
            .map(|offset| offset + sentence[offset..].chars().next().map_or(0, char::len_utf8));
        let request_speech = !cleared && speech.is_none() && boundary.is_some();
        let dialogue = dialogue.clone();
        let result = host
            .call(move |actor| {
                Box::pin(async move {
                    if !actor
                        .turns
                        .get(&dialogue.companion)
                        .is_some_and(|turn| turn.id == dialogue.turn)
                        || (!cleared && !actor.claim(&dialogue))
                    {
                        return Ok((false, None));
                    }
                    actor.emit(Payload::Delta(Box::new(w::TextDelta {
                        companion_id: dialogue.companion.clone(),
                        turn_id: dialogue.turn.clone(),
                        text: delta,
                        reset: cleared,
                        ..Default::default()
                    })));
                    let queue = if request_speech {
                        actor.speech_queue(&dialogue.companion).await?
                    } else {
                        None
                    };
                    Ok((true, queue))
                })
            })
            .await;
        let Ok((true, queue)) = result else {
            break;
        };
        if queue.is_some() {
            speech = queue;
        }
        if let (Some(queue), Some(boundary)) = (&speech, boundary) {
            let text = sentence.drain(..boundary).collect::<String>();
            if queue.send(text).await.is_err() {
                break;
            }
        }
    }
    (speech, sentence)
}

impl Conversation {
    fn prompt(&self) -> Prompt<'_> {
        Prompt {
            system: &self.system,
            summary: &self.summary,
            history: &self.history,
            dynamic: &self.dynamic,
            input: &self.input,
        }
    }
}

fn dynamic(
    mind: &w::Mind,
    memories: &[w::Memory],
    observation: Option<&(String, String)>,
) -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "time": jiff::ZonedDateTime::now().to_string(),
        "mind": mind,
        "perception": observation.map(|(fact, interpretation)| json!({"observation": fact, "interpretation": interpretation})),
        "memories": memories.iter().map(|memory| json!({"id": memory.id, "text": truncate(&memory.text, 550)})).collect::<Vec<_>>(),
    }))?)
}
