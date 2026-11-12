//! Work-session services.
use super::Services;
use crate::host::work;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use ene_rpc as w;

#[allow(refining_impl_trait)]
impl w::WorkService for Services {
    async fn list_works(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ListWorksRequest>,
    ) -> ServiceResult<w::WorkViews> {
        let companion = request.companion_id.to_owned();
        self.read(move |app| {
            Ok(w::WorkViews {
                items: work::list(app.world_mut(), &companion)?,
                ..Default::default()
            })
        })
        .await
    }

    async fn get_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::WorkRequest>,
    ) -> ServiceResult<w::WorkView> {
        let id = request.work_id.to_owned();
        self.read(move |app| work::view(app.world_mut(), &id)).await
    }

    async fn start_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::StartWorkRequest>,
    ) -> ServiceResult<w::WorkSession> {
        let request = request.to_owned_message();
        self.call(move |app| {
            work::start(
                app,
                request.companion_id,
                request.title,
                request.instruction,
                request.client_id,
            )
        })
        .await
    }

    async fn send_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SendWorkRequest>,
    ) -> ServiceResult<w::WorkAcceptance> {
        let request = request.to_owned_message();
        self.call(move |app| work::send(app, request)).await
    }

    async fn answer_inquiry(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::AnswerInquiryRequest>,
    ) -> ServiceResult<w::WorkAcceptance> {
        let request = request.to_owned_message();
        self.call(move |app| work::answer_inquiry(app, request))
            .await
    }

    async fn stop_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::StopWorkRequest>,
    ) -> ServiceResult<w::Empty> {
        let id = request.work_id.to_owned();
        self.call(move |app| {
            work::stop(app, &id)?;
            Ok(w::Empty::default())
        })
        .await
    }

    async fn close_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::WorkRequest>,
    ) -> ServiceResult<w::Empty> {
        let id = request.work_id.to_owned();
        self.call(move |app| {
            work::close(app, &id)?;
            Ok(w::Empty::default())
        })
        .await
    }
}
