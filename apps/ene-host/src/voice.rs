use crate::{
    actor::{Actor, Host, id},
    error::invalid,
    model::{Model, truncate},
    store,
};
use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ene_rpc::{self as w, model_selection::Role};
use rig_core::{
    audio_generation::AudioGenerationRequestBuilder, transcription::TranscriptionRequestBuilder,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

impl Actor {
    pub(crate) fn stop_speech(&mut self) {
        self.speech.cancel();
        self.speech = self.host.shutdown.child_token();
        let _no_receivers = self.audio.send(w::AudioOutput {
            generation: self.activation.generation,
            stop: true,
            ..Default::default()
        });
    }

    pub(crate) async fn speech_queue(
        &mut self,
        companion_id: &str,
    ) -> Result<Option<tokio::sync::mpsc::Sender<String>>> {
        if self.activation.device_id.is_empty() || self.audio.receiver_count() == 0 {
            return Ok(None);
        }
        let companion = self.companion(companion_id).await?;
        let Ok((selection, connection)) = self.selection(&companion, Role::Synthesis).await else {
            return Ok(None);
        };
        let voice = {
            let mut db = self.db.acquire().await?;
            store::get::<w::Character>(&mut db, "character", &companion.character_id)
                .await?
                .map_or_else(String::new, |character| character.voice)
        };
        if voice.is_empty() {
            return Ok(None);
        }
        self.stop_speech();
        let cancel = self.speech.clone();
        let audio = self.audio.clone();
        let generation = self.activation.generation;
        let models = self.models.clone();
        let companion_id = companion_id.to_owned();
        let db = self.db.clone();
        let (out, mut queue) = tokio::sync::mpsc::channel::<String>(8);
        self.jobs.spawn(async move {
            let mut speed = 1.0;
            for decision in crate::plugins::hooks(
                &db,
                ene_plugin::HookEvent::PreSpeech {
                    companion_id: companion_id.clone(),
                },
                &cancel,
            )
            .await
            {
                if let ene_plugin::HookDecision::TuneSpeech { speed: adjusted } = decision
                    && (0.5..=2.0).contains(&adjusted)
                {
                    speed = adjusted;
                }
            }
            let initialized = tokio::select! {
                () = cancel.cancelled() => return,
                model = models.model(selection, connection) => model,
            };
            let Ok(model) = initialized else { return; };
            let utterance_id = id();
            let mut rate_hint = None;
            loop {
                let sentence = tokio::select! {
                    () = cancel.cancelled() => return,
                    text = queue.recv() => text,
                };
                let Some(sentence) = sentence else { break; };
                let response = tokio::select! {
                    () = cancel.cancelled() => return,
                    result = synthesize(&model, truncate(&sentence, 4096), voice.clone(), speed, &cancel) => result,
                };
                let Ok((pcm, rate)) = response else { return; };
                if pcm.len() > 12 * 1024 * 1024 || !pcm.len().is_multiple_of(2)
                    || rate_hint.is_some_and(|expected| expected != rate)
                {
                    return;
                }
                rate_hint = Some(rate);
                for chunk in pcm.chunks(8192) {
                    if cancel.is_cancelled() || audio.send(w::AudioOutput {
                        utterance_id: utterance_id.clone(), companion_id: companion_id.clone(),
                        generation, pcm: chunk.to_vec(), sample_rate: rate, ..Default::default()
                    }).is_err() {
                        return;
                    }
                    let duration = std::time::Duration::from_secs_f64(chunk.len() as f64 / f64::from(rate) / 2.0);
                    tokio::time::sleep(duration.mul_f64(0.8)).await;
                }
            }
            if !cancel.is_cancelled() && let Some(rate) = rate_hint {
                let _closed = audio.send(w::AudioOutput {
                    utterance_id, companion_id: companion_id.clone(),
                    generation, sample_rate: rate, end: true, ..Default::default()
                });
            }
            let _observed = crate::plugins::hooks(
                &db,
                ene_plugin::HookEvent::PostSpeech { companion_id },
                &cancel,
            )
            .await;
        });
        Ok(Some(out))
    }

    pub(crate) async fn speak_if_configured(
        &mut self,
        companion: &str,
        text: String,
    ) -> Result<()> {
        if let Some(queue) = self.speech_queue(companion).await? {
            let _closed = queue.try_send(text);
        }
        Ok(())
    }

    pub(crate) async fn speak(&mut self, request: w::SpeakRequest) -> Result<w::Empty> {
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("speak", &request))?;
        let mut db = self.db.acquire().await?;
        if let Some(saved) = store::replay(&mut db, &key.request_key, &hash).await? {
            return Ok(saved);
        }
        if request.text.trim().is_empty() || request.text.len() > 4096 {
            return Err(invalid("invalid speech text"));
        }
        self.companion(&request.companion_id).await?;
        let reply = w::Empty::default();
        store::acknowledge(&mut db, &key.request_key, &hash, &reply).await?;
        drop(db);
        self.speak_if_configured(&request.companion_id, request.text)
            .await?;
        Ok(reply)
    }
}

impl Host {
    pub(crate) async fn transcribe(
        &self,
        device: String,
        generation: u64,
        rate: u32,
        pcm: Vec<u8>,
        request_key: String,
    ) -> Result<w::Transcription> {
        ensure!(
            (8000..=192_000).contains(&rate)
                && !pcm.is_empty()
                && pcm.len().is_multiple_of(2)
                && pcm.len() <= 4 * 1024 * 1024,
            "invalid PCM input"
        );
        let (models, binding, companions) = self
            .call({
                let device = device.clone();
                move |actor| {
                    Box::pin(async move {
                        actor.active(&device, generation)?;
                        let mut db = actor.db.acquire().await?;
                        let companions: Vec<w::Companion> =
                            store::list(&mut db, "companion").await?;
                        let selected = companions
                            .iter()
                            .find(|companion| {
                                companion
                                    .models
                                    .iter()
                                    .any(|model| model.role == Role::Transcription)
                            })
                            .ok_or_else(|| invalid("no transcription model configured"))?;
                        Ok((
                            actor.models.clone(),
                            actor.selection(selected, Role::Transcription).await?,
                            companions,
                        ))
                    })
                }
            })
            .await?;
        let (selection, connection) = binding;
        let model = models.model(selection, connection).await?;
        let cancel = self.shutdown.child_token();
        let _cancel = cancel.clone().drop_guard();
        let text = transcribe(&model, rate, pcm, &cancel).await?;
        if text.trim().is_empty() {
            return Ok(w::Transcription::default());
        }
        let transcript = text.clone();
        let receipt = self
            .call(move |actor| {
                Box::pin(async move {
                    actor.active(&device, generation)?;
                    actor.send_group(companions, text, &request_key).await
                })
            })
            .await?;
        Ok(w::Transcription {
            text: transcript,
            id: receipt.id,
            ..Default::default()
        })
    }
}

async fn synthesize(
    model: &Model,
    text: &str,
    voice: String,
    speed: f32,
    cancel: &CancellationToken,
) -> Result<(Vec<u8>, u32)> {
    if let Some(plugin) = &model.plugin {
        let (_, events) = plugin
            .invoke(
                ene_plugin::Operation::Synthesize {
                    request: ene_plugin::SynthesisRequest {
                        endpoint: model.connection.endpoint.clone(),
                        options: plugin.options.clone(),
                        model: model.selection.model.clone(),
                        text: text.into(),
                        voice,
                        speed,
                        format: ene_plugin::AudioFormat::Pcm16,
                    },
                },
                cancel,
            )
            .await?;
        let mut rate = 0;
        let mut pcm = Vec::new();
        let mut finished = false;
        for event in events {
            if let ene_plugin::Event::Audio {
                data_base64,
                sample_rate,
                done,
            } = event
            {
                ensure!(
                    !finished && (sample_rate == 0 || rate == 0 || sample_rate == rate),
                    "invalid plugin audio sequence"
                );
                if sample_rate > 0 {
                    rate = sample_rate;
                }
                pcm.extend(STANDARD.decode(data_base64)?);
                ensure!(
                    pcm.len() <= 12 * 1024 * 1024,
                    "plugin returned too much audio"
                );
                finished = done;
            }
        }
        ensure!(
            finished && (8000..=192_000).contains(&rate),
            "plugin did not return complete PCM audio"
        );
        return Ok((pcm, rate));
    }
    let request = AudioGenerationRequestBuilder::new(text, voice)
        .speed(speed)
        .additional_params(json!({"response_format":"pcm"}))
        .build();
    let response = model
        .client
        .audio_generation(&model.selection.model)
        .call(request)
        .await?;
    Ok((response.audio, 24_000))
}

async fn transcribe(
    model: &Model,
    rate: u32,
    pcm: Vec<u8>,
    cancel: &CancellationToken,
) -> Result<String> {
    if let Some(plugin) = &model.plugin {
        let (reply, events) = plugin
            .invoke(
                ene_plugin::Operation::Transcribe {
                    request: ene_plugin::TranscriptionRequest {
                        endpoint: model.connection.endpoint.clone(),
                        options: plugin.options.clone(),
                        model: model.selection.model.clone(),
                        pcm_base64: STANDARD.encode(&pcm),
                        sample_rate: rate,
                        language: None,
                    },
                },
                cancel,
            )
            .await?;
        if let Some(text) = events.into_iter().find_map(|event| match event {
            ene_plugin::Event::Transcription {
                text,
                is_final: true,
            } => Some(text),
            _ => None,
        }) {
            return Ok(text);
        }
        let reply: ene_plugin::TranscriptionResult = serde_json::from_value(reply)?;
        return Ok(reply.text);
    }
    let request = TranscriptionRequestBuilder::new(wav(rate, &pcm))
        .filename(Some("input.wav".into()))
        .build();
    Ok(model
        .client
        .transcription(&model.selection.model)
        .call(request)
        .await?
        .text)
}

fn wav(rate: u32, pcm: &[u8]) -> Vec<u8> {
    let size = pcm.len() as u32;
    let mut data = Vec::with_capacity(44 + pcm.len());
    data.extend(b"RIFF");
    data.extend((36 + size).to_le_bytes());
    data.extend(b"WAVEfmt ");
    data.extend(16u32.to_le_bytes());
    data.extend([1, 0, 1, 0]);
    data.extend(rate.to_le_bytes());
    data.extend((rate * 2).to_le_bytes());
    data.extend([2, 0, 16, 0]);
    data.extend(b"data");
    data.extend(size.to_le_bytes());
    data.extend(pcm);
    data
}
