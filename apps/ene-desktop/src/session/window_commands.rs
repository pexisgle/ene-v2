//! The window-command half of the session: one handler per UI command.
//!
//! These are free functions rather than methods because the session already
//! spans three inherent `impl` files; each takes the session it acts on.

use super::stream::StreamLoopState;
use super::{Session, Step};
use crate::update::Update;
use crate::{host::Host, perception::Sensor, update::Command};
use ene_rpc as w;

/// Handles one window command.
pub(super) async fn command(
    session: &mut Session,
    host: &Host,
    stream: &mut StreamLoopState,
    command: Command,
) -> Step {
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
            stream
                .publication
                .refresh(&mut session.transport, host, &session.devices);
            Step::Continue
        }
        Command::SetEyesEnabled(enabled) => {
            session.devices.set_eyes_enabled(enabled, &session.updates);
            if !enabled && stream.pause_eyes() {
                session.transport.advance_eyes_generation();
                let event = session
                    .transport
                    .perception_event(w::perception_event::Event::Enabled(false));
                let _reported = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    host.client.report_perception(event),
                )
                .await;
            }
            stream
                .publication
                .refresh(&mut session.transport, host, &session.devices);
            Step::Continue
        }
        #[cfg(windows)]
        Command::SetComputerUseEnabled(enabled) => {
            session
                .devices
                .set_computer_use_enabled(enabled, &session.updates);
            stream
                .publication
                .refresh(&mut session.transport, host, &session.devices);
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
            session.audio.handle_ptt(&command, &session.updates);
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
    if !session.transport.bindings.contains(&companion_id) {
        match session.transport.activate_one(host, &companion_id).await {
            Ok(()) => {
                session.transport.bindings.insert(companion_id);
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
    if !session.transport.bindings.contains(&companion_id) {
        match session.transport.activate_one(host, &companion_id).await {
            Ok(()) => {
                session.transport.bindings.insert(companion_id.clone());
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
        session.transport.focused = session.transport.bindings.first().cloned();
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
    let (Some(id), Some(avatar)) = (
        session.transport.focused_companion_id(),
        session.devices.avatar.as_mut(),
    ) else {
        super::notify_ui(
            &session.updates,
            Update::Failed {
                context: "アバターのサイズ変更",
                message: "アバター描画プロセスまたはコンパニオンが設定されていません".into(),
            },
        );
        return Step::Continue;
    };
    let body = session.transport.body_binding_for(id);
    let Some(model) = session.transport.resolve_model(&body) else {
        return Step::Continue;
    };
    match avatar.set_scale(id, &model, scale).await {
        Ok(()) => {
            super::notify_ui(&session.updates, Update::AvatarScaleChanged(scale));
            session.report_devices().await;
        }
        Err(error) => super::notify_ui(
            &session.updates,
            Update::Failed {
                context: "アバターのサイズ変更",
                message: error.to_string(),
            },
        ),
    }
    Step::Continue
}

fn cmd_speak(session: &Session, host: &Host, companion_id: Option<String>, text: String) {
    let host = host.clone();
    let updates = session.updates.clone();
    let client_id = session.transport.options.id.clone();
    // Synthesis is a network call; awaiting it here would stall the
    // microphone and the playback this loop owns.
    let mut background = session.devices.background.lock();
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
