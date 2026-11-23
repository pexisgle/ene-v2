//! Work-session services.
use super::Services;
use crate::host::{db::Store, work};
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use ene_rpc as w;

#[expect(refining_impl_trait, reason = "Connect boxes futures")]
impl w::WorkService for Services {
    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "list_works",
            companion_id = %request.companion_id,
            request_id = tracing::field::Empty,
        )
    )]
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

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "get_work",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            work_id = %request.work_id,
        )
    )]
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
    discard_id!(stop_work, w::StopWorkRequest, work_id, work::stop);
    discard_id!(close_work, w::WorkRequest, work_id, work::close);
}
