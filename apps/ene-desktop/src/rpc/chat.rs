//! Snapshot, chat history, message sending, and turn cancellation RPCs.

use super::RpcDispatcher;
use crate::update::Update;
use ene_client_core::domain::Message;
use ene_rpc as w;

pub(crate) fn resnapshot(rpc: &RpcDispatcher, epoch: u64) {
    rpc.call_owned(
        epoch,
        "ホスト状態の取得",
        request!(host.get_snapshot(w::Empty::default())),
        |snapshot| Update::Notification(Box::new(snapshot.into())),
    );
}

pub(crate) fn load_history(rpc: &RpcDispatcher, epoch: u64, companion_id: String) -> bool {
    let target = companion_id.clone();
    let failed_target = companion_id.clone();
    rpc.call_with(
        epoch,
        "会話履歴の取得",
        |host| async move {
            let history = host
                .chat
                .get_history(w::HistoryRequest {
                    companion_id,
                    limit: Some(100),
                    ..Default::default()
                })
                .await?
                .into_owned();
            let observed_at = w::client_model::host_cursor(history.observed_at.into_option());
            let messages = history
                .items
                .into_iter()
                .map(Message::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| connectrpc::ConnectError::internal(error.to_string()))?;
            Ok((messages, observed_at))
        },
        move |(messages, observed_at)| Update::History {
            companion_id: target,
            messages,
            observed_at,
        },
        move |_, error| Update::HistoryFailed {
            companion_id: failed_target,
            message: error.to_string(),
        },
    )
}

pub(crate) fn send_message(
    rpc: &RpcDispatcher,
    epoch: u64,
    companion_id: String,
    request_key: String,
    text: String,
) -> bool {
    let accepted_key = request_key.clone();
    let failed_key = request_key.clone();
    rpc.call_with(
        epoch,
        "メッセージの送信",
        |host| async move {
            host.chat
                .send_message(w::SendMessageRequest {
                    companion_id,
                    request_key,
                    text,
                    ..Default::default()
                })
                .await?;
            Ok(())
        },
        move |()| Update::Accepted {
            request_key: accepted_key,
        },
        move |_, error| Update::SendFailed {
            request_key: failed_key,
            reason: error.to_string(),
        },
    )
}

pub(crate) fn stop(rpc: &RpcDispatcher, epoch: u64, companion_id: String) {
    let stopping = companion_id.clone();
    rpc.call(
        epoch,
        "発話・生成の停止",
        request!(chat.stop(w::StopRequest {
            companion_id,
            ..Default::default()
        })),
        move |_| Update::StopRequested {
            companion_id: stopping,
        },
    );
}
