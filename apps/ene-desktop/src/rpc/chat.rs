//! Snapshot, chat history, message sending, and turn cancellation RPCs.

use super::{RpcDispatcher, decode};
use crate::update::Update;
use buffa::view::MessageView;
use ene_client_core::domain::{HostNotification, Message};
use ene_domain::CompanionId;
use ene_rpc as w;

/// The typed form of a companion id the window selected.
///
/// The window holds the id as a `String` its dropdowns already carry, so the
/// parse happens here, at the boundary, rather than in `ui/`.
pub(crate) fn parse_companion_id(id: &str) -> Option<CompanionId> {
    CompanionId::parse(id).ok()
}

pub(crate) fn resnapshot(rpc: &RpcDispatcher, epoch: u64) {
    let Some(host) = rpc.host() else {
        return;
    };
    rpc.spawn(
        epoch,
        "ホスト状態の取得",
        async move {
            let response = host.host.get_snapshot(w::Empty::default()).await?;
            response.view().to_owned_message().map_err(decode)
        },
        |snapshot| Update::Notification(Box::new(HostNotification::initial_state(snapshot.into()))),
    );
}

pub(crate) fn load_history(rpc: &RpcDispatcher, epoch: u64, companion_id: String) -> bool {
    let Some(host) = rpc.host() else {
        return false;
    };
    let target = companion_id.clone();
    let failed_target = companion_id.clone();
    rpc.spawn_with(
        epoch,
        "会話履歴の取得",
        async move {
            let response = host
                .chat
                .get_history(w::HistoryRequest {
                    companion_id,
                    limit: Some(100),
                    ..Default::default()
                })
                .await?;
            let history = response.view().to_owned_message().map_err(decode)?;
            history
                .items
                .into_iter()
                .map(Message::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| connectrpc::ConnectError::internal(error.to_string()))
        },
        move |messages| Update::History {
            companion_id: target,
            messages,
        },
        move |_, error| Update::HistoryFailed {
            companion_id: failed_target,
            message: error.to_string(),
        },
    );
    true
}

pub(crate) fn send_message(
    rpc: &RpcDispatcher,
    epoch: u64,
    companion_id: String,
    request_key: String,
    text: String,
) -> bool {
    let Some(host) = rpc.host() else {
        return false;
    };
    rpc.spawn_update(epoch, async move {
        let outcome = match host
            .chat
            .send_message(w::SendMessageRequest {
                companion_id,
                request_key: request_key.clone(),
                text: text.clone(),
                client_id: String::new(),
                ..Default::default()
            })
            .await
        {
            Ok(response) => response
                .view()
                .to_owned_message()
                .map(|acceptance| acceptance.request_id)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        match outcome {
            Ok(request_id) => Update::Accepted {
                request_key,
                request_id,
            },
            Err(reason) => Update::SendFailed {
                request_key,
                text,
                reason,
            },
        }
    });
    true
}

pub(crate) fn stop(rpc: &RpcDispatcher, epoch: u64, companion_id: String) {
    let Some(host) = rpc.host() else {
        return;
    };
    let stopping = companion_id.clone();
    rpc.spawn(
        epoch,
        "発話・生成の停止",
        async move {
            host.chat
                .stop(w::StopRequest {
                    companion_id,
                    ..Default::default()
                })
                .await
        },
        move |_| Update::StopRequested {
            companion_id: stopping,
        },
    );
}
