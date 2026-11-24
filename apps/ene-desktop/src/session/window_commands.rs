//! The window-command half of the session: one handler per UI command.
//!
//! These are free functions rather than methods because the session already
//! spans three inherent `impl` files; each takes the session it acts on.

use super::{Session, Step};
use crate::update::Update;
use crate::{host::Host, perception::Sensor, update::Command};
use ene_rpc as w;
use std::path::PathBuf;

/// Handles one window command.
pub(super) async fn command(session: &mut Session, host: &Host, command: Command) -> Step {
    match command {
        Command::Serve(companion_id) => cmd_serve(session, host, companion_id).await,
        Command::ActivateCompanion(companion_id) => {
            cmd_activate_companion(session, host, companion_id).await
        }
        Command::ReleaseCompanion(companion_id) => {
            cmd_release_companion(session, host, companion_id).await
        }
        Command::StopPlayback => {
            session.audio.stop_playback();
            Step::Continue
        }
        Command::Refresh => {
            session
                .transport
                .sync_capabilities(host, &session.devices)
                .await;
            Step::Continue
        }
        Command::SetEyesEnabled(enabled) => {
            session.devices.set_eyes_enabled(enabled, &session.updates);
            session
                .transport
                .sync_capabilities(host, &session.devices)
                .await;
            Step::Continue
        }
        #[cfg(windows)]
        Command::SetComputerUseEnabled(enabled) => {
            session
                .devices
                .set_computer_use_enabled(enabled, &session.updates);
            session
                .transport
                .sync_capabilities(host, &session.devices)
                .await;
            Step::Continue
        }
        Command::SetMicMuted(muted) => {
            session.audio.set_mic_muted(muted, &session.updates);
            Step::Continue
        }
        Command::SetCaptureMode(mode) => {
            session.audio.set_capture_mode(mode, &session.updates);
            Step::Continue
        }
        Command::PttStart | Command::PttCommit | Command::PttCancel => {
            session.audio.handle_ptt(command, &session.updates);
            Step::Continue
        }
        Command::SetOutputMuted(muted) => {
            session.audio.set_output_muted(muted, &session.updates);
            Step::Continue
        }
        Command::SetAvatarScale(scale) => cmd_set_avatar_scale(session, scale).await,
        Command::Speak { companion_id, text } => {
            cmd_speak(session, host, companion_id, text);
            Step::Continue
        }
        Command::CenterAvatar => {
            cmd_center_avatar(session).await;
            Step::Continue
        }
        Command::Quit => Step::Stop,
    }
}

async fn cmd_serve(session: &mut Session, host: &Host, companion_id: String) -> Step {
    let was_empty = session.transport.bindings.is_empty();
    session.transport.focused = Some(companion_id.clone());
    if !session.transport.bindings.contains_key(&companion_id) {
        match session.transport.activate_one(host, &companion_id).await {
            Ok(binding) => {
                session.transport.bindings.insert(companion_id, binding);
                session.transport.may_activate = false;
                session.sync_avatars().await;
                if was_empty {
                    return Step::Restart;
                }
            }
            Err(error) => {
                super::notify_ui(
                    &session.updates,
                    Update::Failed {
                        context: "コンパニオンの有効化",
                        message: error.to_string(),
                    },
                );
            }
        }
    }
    Step::Continue
}

async fn cmd_activate_companion(session: &mut Session, host: &Host, companion_id: String) -> Step {
    let was_empty = session.transport.bindings.is_empty();
    if !session.transport.bindings.contains_key(&companion_id) {
        match session.transport.activate_one(host, &companion_id).await {
            Ok(binding) => {
                session
                    .transport
                    .bindings
                    .insert(companion_id.clone(), binding);
                if session.transport.focused.is_none() {
                    session.transport.focused = Some(companion_id);
                }
                session.transport.may_activate = false;
                session.sync_avatars().await;
                if was_empty {
                    return Step::Restart;
                }
            }
            Err(error) => {
                super::notify_ui(
                    &session.updates,
                    Update::Failed {
                        context: "コンパニオンの常駐",
                        message: error.to_string(),
                    },
                );
            }
        }
    }
    Step::Continue
}

async fn cmd_release_companion(session: &mut Session, host: &Host, companion_id: String) -> Step {
    if let Err(error) = session.transport.release_one(host, &companion_id).await {
        super::notify_ui(
            &session.updates,
            Update::Failed {
                context: "コンパニオンの常駐解除",
                message: error.to_string(),
            },
        );
        return Step::Continue;
    }
    session.transport.bindings.remove(&companion_id);
    session.audio.clear_speaking_if(&companion_id);
    if session.transport.focused.as_deref() == Some(companion_id.as_str()) {
        session.transport.focused = session.transport.bindings.keys().next().cloned();
    }
    session.sync_avatars().await;
    if session.transport.bindings.is_empty() {
        session.devices.sensor = Sensor::default();
        Step::Restart
    } else {
        Step::Continue
    }
}

async fn cmd_set_avatar_scale(session: &mut Session, scale: f32) -> Step {
    let scale = scale.clamp(50., 200.);
    session.devices.avatar_scale = scale;
    let target_ids: Vec<String> = session
        .transport
        .focused_companion_id()
        .map(|id| vec![id.to_owned()])
        .unwrap_or_else(|| session.transport.bindings.keys().cloned().collect());
    if target_ids.is_empty() || session.devices.avatar.is_none() {
        super::notify_ui(
            &session.updates,
            Update::Failed {
                context: "アバターのサイズ変更",
                message: "アバター描画プロセスまたはコンパニオンが設定されていません".into(),
            },
        );
        return Step::Continue;
    }
    let models: Vec<(String, Option<PathBuf>)> = target_ids
        .into_iter()
        .map(|id| {
            let model = session.transport.model_for(&id);
            (id, model)
        })
        .collect();
    let mut updated = false;
    let mut last_err = None;
    if let Some(avatar) = session.devices.avatar.as_mut() {
        for (id, model) in models {
            if let Some(model) = model {
                match avatar.set_scale(&id, &model, scale).await {
                    Ok(()) => updated = true,
                    Err(error) => last_err = Some(error),
                }
            }
        }
    }
    if let Some(error) = last_err
        && !updated
    {
        super::notify_ui(
            &session.updates,
            Update::Failed {
                context: "アバターのサイズ変更",
                message: error.to_string(),
            },
        );
    } else if updated {
        super::notify_ui(&session.updates, Update::AvatarScaleChanged(scale));
        session.report_devices().await;
    }
    Step::Continue
}

fn cmd_speak(session: &Session, host: &Host, companion_id: Option<String>, text: String) {
    let host = host.clone();
    let updates = session.updates.clone();
    let client_id = session.transport.options.id.clone();
    // Synthesis is a network call; awaiting it here would stall the
    // microphone and the playback this loop owns.
    let Ok(mut background) = session.devices.background.lock() else {
        return;
    };
    background.spawn(async move {
        let update = match host
            .voice
            .speak(w::SpeakRequest {
                client_id,
                text,
                companion_id,
                ..Default::default()
            })
            .await
        {
            Ok(_) => Update::Spoke,
            Err(error) => Update::SpokeFailed {
                message: error.to_string(),
            },
        };
        super::notify_ui(&updates, update);
    });
}

async fn cmd_center_avatar(session: &mut Session) {
    let target = session.transport.focused_companion_id().map(str::to_owned);
    if let (Some(avatar), Some(target)) = (session.devices.avatar.as_mut(), target) {
        if let Err(error) = avatar.center(&target).await {
            super::notify_ui(
                &session.updates,
                Update::Failed {
                    context: "アバターの中央移動",
                    message: error.to_string(),
                },
            );
        }
    } else {
        super::notify_ui(
            &session.updates,
            Update::Failed {
                context: "アバターの中央移動",
                message: "アバター描画プロセスが起動していません".into(),
            },
        );
    }
}
