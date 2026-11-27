//! The per-character half of [`Avatar`]: what each character shows and how to
//! change it.

use super::client::{Avatar, AvatarError, ModelState, ShownCharacter, WriteItem, lip_sync_line};
use super::presets::{idle_return_preset, is_looping_motion, is_persistent_idle, motion};
use ene_avatar_ipc::{AvatarEvent, CharacterConfig, Command};
use std::path::Path;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;

impl Avatar {
    pub fn shown_characters(&self) -> Vec<String> {
        self.shown.keys().cloned().collect()
    }

    /// Removes `character` from the renderer when its companion is released.
    pub async fn remove_character(&mut self, character: &str) -> Result<(), AvatarError> {
        if self.shown.remove(character).is_some() {
            self.call(Command::Remove {
                character: character.to_owned(),
            })
            .await?;
        }
        Ok(())
    }

    /// Shows `character` applying a `Companion.body` configuration (`model`, `monitor`,
    /// `position`, `size_percent`).
    pub async fn show_with_body(
        &mut self,
        character: &str,
        model: &Path,
        motion: Option<&Path>,
        body: &ene_domain::BodyBinding,
        default_scale: f32,
    ) -> Result<bool, AvatarError> {
        let existing = self.shown.get(character).map(|shown| &shown.config);
        let mut next = existing
            .cloned()
            .unwrap_or_else(|| CharacterConfig::for_model(model.to_string_lossy()));
        next.model = model.to_string_lossy().into_owned();
        if existing.is_none() {
            next.motion = motion.map(|p| p.to_string_lossy().into_owned());
            next.looping = true;
        }
        if body.monitor.is_some() {
            next.monitor = body.monitor.clone();
        }
        if body.position.is_some() {
            next.position = body.position;
        }
        next.size_percent = body.size_percent.unwrap_or(default_scale).clamp(50., 200.);
        if existing == Some(&next) {
            return Ok(false);
        }
        let loads_model = existing.is_none_or(|shown| shown.model != next.model);
        self.configure(character, next, Some(body)).await?;
        Ok(loads_model)
    }

    /// Shows `motion` on `character`, adding it first if it is not there.
    pub async fn show(
        &mut self,
        character: &str,
        model: &Path,
        motion: Option<&Path>,
        looping: bool,
    ) -> Result<bool, AvatarError> {
        let motion = motion.map(|path| path.to_string_lossy().into_owned());
        let existing = self.shown.get(character).map(|shown| &shown.config);
        let mut next = existing
            .cloned()
            .unwrap_or_else(|| CharacterConfig::for_model(model.to_string_lossy()));
        next.model = model.to_string_lossy().into_owned();
        next.motion = motion;
        next.looping = looping;
        if looping && existing == Some(&next) {
            return Ok(false);
        }
        let loads_model = existing.is_none_or(|shown| shown.model != next.model);
        if loads_model {
            self.configure(character, next, None).await?;
        } else {
            self.call(Command::Motion {
                character: character.to_owned(),
                motion: next.motion.clone(),
                looping,
            })
            .await?;
            if let Some(shown) = self.shown.get_mut(character) {
                shown.config = next;
            }
        }
        self.drain_incoming_events();
        self.events.retain(|event| {
            !(matches!(event, AvatarEvent::Finished { .. }) && event.character() == character)
        });
        Ok(loads_model)
    }

    /// Shows a declared motion preset, tracking looping resting postures for return-to-idle.
    pub async fn show_preset(
        &mut self,
        character: &str,
        model: &Path,
        preset: &str,
        clip: Option<&Path>,
    ) -> Result<bool, AvatarError> {
        let looping = is_looping_motion(preset);
        let loads_model = self.show(character, model, clip, looping).await?;
        if let Some(shown) = self.shown.get_mut(character) {
            shown.last_motion = Some(preset.to_owned());
            if is_persistent_idle(preset) {
                shown.base_motion = Some(preset.to_owned());
            }
        }
        Ok(loads_model)
    }

    pub async fn return_to_idle(
        &mut self,
        character: &str,
        model: &Path,
    ) -> Result<(), AvatarError> {
        if let Some(shown) = self
            .shown
            .get(character)
            .filter(|shown| !shown.config.looping)
        {
            let preset =
                idle_return_preset(shown.base_motion.as_deref(), shown.last_motion.as_deref());
            self.show_preset(character, model, preset, motion(preset).as_deref())
                .await?;
        }
        Ok(())
    }

    async fn configure(
        &mut self,
        character: &str,
        next: CharacterConfig,
        placement: Option<&ene_domain::BodyBinding>,
    ) -> Result<(), AvatarError> {
        let command = if self.shown.contains_key(character) {
            Command::Update {
                character: character.to_owned(),
                config: CharacterConfig {
                    monitor: placement.and_then(|body| body.monitor.clone()),
                    position: placement.and_then(|body| body.position),
                    ..next.clone()
                },
            }
        } else {
            Command::Add {
                character: character.to_owned(),
                config: next.clone(),
            }
        };
        self.call(command).await?;
        if let Some(shown) = self.shown.get_mut(character) {
            if shown.config.model != next.model {
                shown.state = ModelState::Loading;
            }
            shown.config = next;
        } else {
            self.shown.insert(
                character.to_owned(),
                ShownCharacter::new(next, ModelState::Loading),
            );
        }
        Ok(())
    }

    /// Sets the character's face, or clears it with `None`.
    pub async fn expression(
        &mut self,
        character: &str,
        name: Option<&str>,
    ) -> Result<(), AvatarError> {
        self.call(Command::Expression {
            character: character.to_owned(),
            name: name.map(str::to_owned),
        })
        .await?;
        Ok(())
    }

    /// Updates the character's display scale while keeping motion and placement.
    pub async fn set_scale(
        &mut self,
        character: &str,
        model: &Path,
        size_percent: f32,
    ) -> Result<(), AvatarError> {
        let clamped = size_percent.clamp(50., 200.);
        let next = match self.shown.get(character) {
            Some(current) => current.config.with_scale(clamped),
            None => CharacterConfig::for_model(model.to_string_lossy()).with_scale(clamped),
        };
        self.configure(character, next, None).await
    }

    /// Centers the character on the primary monitor.
    pub async fn center(&mut self, character: &str) -> Result<(), AvatarError> {
        self.call(Command::Center {
            character: character.to_owned(),
            monitor: None,
        })
        .await?;
        Ok(())
    }

    /// Sets or clears direct gaze angles in degrees (`None` returns gaze to animation/neutral).
    pub async fn look_at(
        &mut self,
        character: &str,
        yaw_deg: Option<f32>,
        pitch_deg: Option<f32>,
    ) -> Result<(), AvatarError> {
        self.call(Command::LookAt {
            character: character.to_owned(),
            yaw_deg,
            pitch_deg,
        })
        .await?;
        Ok(())
    }

    /// Places the character's foot position on an optional monitor (logical pixels).
    pub async fn place(
        &mut self,
        character: &str,
        monitor: Option<String>,
        position: [f32; 2],
    ) -> Result<(), AvatarError> {
        self.call(Command::Place {
            character: character.to_owned(),
            monitor: monitor.clone(),
            position,
        })
        .await?;
        if let Some(shown) = self.shown.get_mut(character) {
            if monitor.is_some() {
                shown.config.monitor = monitor;
            }
            shown.config.position = Some(position);
        }
        Ok(())
    }

    /// Queues one mouth amplitude non-blockingly; drops frames when congested.
    pub fn lip_sync(&mut self, character: &str, rms: f32) -> Result<(), AvatarError> {
        if !self.is_healthy() {
            return Err(AvatarError::Unhealthy);
        }
        let Some(write_tx) = &self.write_tx else {
            return Err(AvatarError::InputClosed);
        };
        let line = lip_sync_line(character, rms)?;
        match write_tx.try_send(WriteItem::OneWay(line)) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.healthy.store(false, Ordering::Release);
                Err(AvatarError::InputClosed)
            }
        }
    }

    pub fn is_ready(&self, character: &str) -> bool {
        self.is_healthy()
            && self
                .shown
                .get(character)
                .is_some_and(|shown| matches!(shown.state, ModelState::Ready))
    }

    pub fn failure(&self, character: &str) -> Option<&str> {
        match &self.shown.get(character)?.state {
            ModelState::Failed(error) => {
                Some(error.as_deref().unwrap_or("モデルの読込に失敗しました"))
            }
            _ => None,
        }
    }

    /// What the renderer is showing, for the device footer.
    pub fn describe(&self, character: &str) -> Option<String> {
        self.shown
            .get(character)
            .filter(|shown| self.is_healthy() && matches!(shown.state, ModelState::Ready))
            .map(|shown| {
                let shown = &shown.config;
                let model = shown
                    .model
                    .rsplit(['\\', '/'])
                    .next()
                    .unwrap_or(&shown.model)
                    .to_owned();
                match shown
                    .motion
                    .as_ref()
                    .and_then(|motion| motion.rsplit(['\\', '/']).next())
                {
                    Some(motion) => format!("{model} ({motion})"),
                    None => model,
                }
            })
    }
}
