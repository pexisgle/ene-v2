//! Chat service: message acceptance, history and turns.
use super::Services;
use crate::host::chat;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use ene_rpc as w;

#[allow(refining_impl_trait)]
impl w::ChatService for Services {
    async fn send_message(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SendMessageRequest>,
    ) -> ServiceResult<w::Acceptance> {
        let request = request.to_owned_message();
        self.call(move |app| chat::accept(app, request)).await
    }

    async fn stop(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::StopRequest>,
    ) -> ServiceResult<w::Empty> {
        let companion = request.companion_id.to_owned();
        self.call(move |app| {
            chat::stop(app, &companion)?;
            Ok(w::Empty::default())
        })
        .await
    }

    async fn get_history(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::HistoryRequest>,
    ) -> ServiceResult<w::History> {
        let request = request.to_owned_message();
        self.read(move |app| chat::history(app.world_mut(), request))
            .await
    }

    async fn get_message(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::MessageRequest>,
    ) -> ServiceResult<w::Message> {
        let request = request.to_owned_message();
        self.read(move |app| chat::message(app.world_mut(), request))
            .await
    }
}
