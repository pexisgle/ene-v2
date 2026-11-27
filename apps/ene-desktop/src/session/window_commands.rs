//! The window-command half of the session: one handler per UI command.
//!
//! These are free functions rather than methods because the session already
//! spans three inherent `impl` files; each takes the session it acts on.

use super::stream::StreamLoopState;
use super::transport::change_activation;
use super::{Session, Step};
use crate::update::Update;
use crate::{host::Host, update::Command};
use ene_rpc as w;

/// Handles one window command.
pub(super) async fn command(
    session: &mut Session,
    host: &Host,
    stream: &mut StreamLoopState,
    command: Command,
) -> Step {
    match command {
        Command::Serve(companion_id) => {
            request_activation(session, host, stream, companion_id, Activation::Follow)
        }
        Command::ActivateCompanion(companion_id) => {
            request_activation(session, host, stream, companion_id, Activation::Keep)
        }
        Command::ReleaseCompanion(companion_id) => {
            request_activation(session, host, stream, companion_id, Activation::Release)
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

#[derive(Clone, Copy)]
enum Activation {
    Follow,
    Keep,
    Release,
}

fn request_activation(
    session: &mut Session,
    host: &Host,
    stream: &mut StreamLoopState,
    companion_id: String,
    change: Activation,
) -> Step {
    if matches!(change, Activation::Follow) {
        session.transport.focused = Some(companion_id.clone());
    }
    let release = matches!(change, Activation::Release);
    if !release && session.transport.bindings.contains(&companion_id) {
        return Step::Continue;
    }
    let context = match change {
        Activation::Follow => "コンパニオンの有効化",
        Activation::Keep => "コンパニオンの常駐",
        Activation::Release => "コンパニオンの常駐解除",
    };
    if let Some(Err(error)) = stream.activation.try_join_next() {
        tracing::warn!(%error, "the activation request stopped");
    }
    if !stream.activation.is_empty() {
        super::notify_ui(
            &session.updates,
            Update::failed(
                context,
                "処理中の切り替えが完了してから、もう一度操作してください。",
            ),
        );
        return Step::Continue;
    }
    let client = (!release).then(|| session.transport.options.id.clone());
    let host = host.clone();
    let updates = session.updates.clone();
    stream.activation.spawn(async move {
        if let Err(error) = change_activation(&host, &companion_id, client.as_deref()).await {
            super::notify_ui(&updates, Update::failed(context, error.to_string()));
        }
    });
    Step::Continue
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
            Update::failed(
                "アバターのサイズ変更",
                "アバター描画プロセスまたはコンパニオンが設定されていません",
            ),
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
            Update::failed("アバターのサイズ変更", error.to_string()),
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
                Update::failed("アバターの中央移動", error.to_string()),
            );
        }
    } else {
        super::notify_ui(
            &session.updates,
            Update::failed(
                "アバターの中央移動",
                "アバター描画プロセスが起動していません",
            ),
        );
    }
}
