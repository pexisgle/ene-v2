//! Conversation selection, history loading, sending, retries, and stop for [`Hub`].

use super::Hub;
use crate::{
    rpc,
    state::{Chat, NoticeKind},
    update::Command,
};
use ene_domain::CompanionId;
use gpui::Context;

pub(super) fn select_companion(hub: &mut Hub, companion_id: &str, cx: &mut Context<Hub>) {
    if hub.model.companion_id() == Some(companion_id) {
        return;
    }
    hub.model.companions.selected = Some(companion_id.to_owned());
    hub.model.companions.chat = Chat {
        companion_id: CompanionId::parse(companion_id).ok(),
        loading: true,
        ..Default::default()
    };
    hub.model.companions.streaming.clear();
    hub.model.audio.transcription = None;
    hub.command(Command::Serve(companion_id.to_owned()));
    hub.load_history(cx);
    hub.load_mind(cx);
    hub.windows.pod_scroll_ping = hub.windows.pod_scroll_ping.saturating_add(1);
    cx.notify();
}

pub(super) fn load_history(hub: &mut Hub, _cx: &mut Context<Hub>) {
    let Some(companion_id) = hub.model.companion_id().map(str::to_owned) else {
        return;
    };
    if !hub.live() {
        hub.model.companions.chat.loading = false;
        return;
    }
    if !rpc::chat::load_history(
        &hub.rpc,
        hub.model.connections.link_epoch,
        companion_id.clone(),
    ) {
        return;
    }
    let target = CompanionId::parse(&companion_id).ok();
    if hub
        .model
        .companions
        .chat
        .companion_id
        .as_ref()
        .is_none_or(|id| id.as_str() != companion_id.as_str())
    {
        hub.model.companions.chat = Chat {
            companion_id: target,
            loading: true,
            ..Default::default()
        };
    } else {
        hub.model.companions.chat.loading = true;
    }
}

pub(super) fn send(hub: &mut Hub, text: String, cx: &mut Context<Hub>) -> bool {
    let request_key = uuid::Uuid::new_v4().to_string();
    hub.send_keyed(text, request_key, cx)
}

pub(super) fn send_keyed(
    hub: &mut Hub,
    text: String,
    request_key: String,
    cx: &mut Context<Hub>,
) -> bool {
    let text = text.trim().to_owned();
    if text.is_empty() {
        return false;
    }
    if !hub.live() {
        hub.model.notice(
            NoticeKind::Failed,
            "メッセージの送信",
            "ホストにまだ接続されていません。",
        );
        cx.notify();
        return false;
    }
    let Some(companion) = hub.model.companion_id().map(str::to_owned) else {
        hub.model.notice(
            NoticeKind::Failed,
            "メッセージの送信",
            "会話するコンパニオンが選択されていません。",
        );
        cx.notify();
        return false;
    };
    if hub.rpc.host().is_none() {
        return false;
    }
    hub.model
        .companions
        .chat
        .sending
        .push(crate::state::Sending {
            request_key: request_key.clone(),
            text: text.clone(),
        });
    tracing::info!(
        companion = %companion,
        key = %request_key,
        chars = text.chars().count(),
        "message sent"
    );
    if !rpc::chat::send_message(
        &hub.rpc,
        hub.model.connections.link_epoch,
        companion,
        request_key,
        text,
    ) {
        // No host is bound, so nothing was queued; the composer keeps the
        // text and the caller's `false` return tells the window to stay.
        tracing::debug!("no host is connected; the message was not queued");
        return false;
    }
    hub.windows.pod_scroll_ping = hub.windows.pod_scroll_ping.saturating_add(1);
    cx.notify();
    true
}

pub(super) fn retry_unsent(hub: &mut Hub, request_key: &str, text: &str, cx: &mut Context<Hub>) {
    hub.model
        .companions
        .chat
        .unsent
        .retain(|item| item.request_key != request_key);
    let _discarded = hub.send_keyed(text.to_owned(), request_key.to_owned(), cx);
}

pub(super) fn dismiss_unsent(hub: &mut Hub, request_key: &str, cx: &mut Context<Hub>) {
    hub.model
        .companions
        .chat
        .unsent
        .retain(|item| item.request_key != request_key);
    cx.notify();
}

pub(super) fn stop(hub: &mut Hub, cx: &mut Context<Hub>) {
    hub.command(Command::StopPlayback);
    if let Some(companion) = hub.model.companion_id().map(str::to_owned) {
        rpc::chat::stop(&hub.rpc, hub.model.connections.link_epoch, companion);
    }
    cx.notify();
}
