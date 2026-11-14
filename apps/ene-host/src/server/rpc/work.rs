//! Work-session services.
use super::Services;
use crate::host::{db::Store, work};
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
            let store = app.world().resource::<Store>().clone();
            work::list(&store, &companion)
        })
        .await
    }

    async fn get_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::WorkRequest>,
    ) -> ServiceResult<w::WorkView> {
        let id = request.work_id.to_owned();
        self.read(move |app| {
            let store = app.world().resource::<Store>().clone();
            work::get(&store, &id)
        })
        .await
    }

    request!(start_work, w::StartWorkRequest, w::WorkSession, work::start);
    request!(send_work, w::SendWorkRequest, w::WorkAcceptance, work::send);
    request!(
        answer_inquiry,
        w::AnswerInquiryRequest,
        w::WorkAcceptance,
        work::answer_inquiry
    );
    discard_id!(stop_work, w::StopWorkRequest, work_id, work::stop);
    discard_id!(close_work, w::WorkRequest, work_id, work::close);
}
