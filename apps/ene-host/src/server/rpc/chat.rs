//! Chat service: message acceptance, history and turns.
use super::Services;
use crate::host::chat;
use ene_rpc as w;

#[allow(refining_impl_trait)]
impl w::ChatService for Services {
    request!(app send_message, w::SendMessageRequest, w::Acceptance, chat::accept);
    discard!(stop, w::StopRequest, chat::stop);
    request!(read get_history, w::HistoryRequest, w::History, chat::history);
    request!(read get_message, w::MessageRequest, w::Message, chat::message_by_id);
}
