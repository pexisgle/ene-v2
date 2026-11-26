//! Work-session services.
use super::Services;
use crate::host::work;
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
        self.with_store(move |store| work::list(&store, &companion))
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
        self.with_store(move |store| work::get(&store, &id)).await
    }

    #[tracing::instrument(name = "rpc", skip_all, fields(method = "start_work", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
    async fn start_work(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::StartWorkRequest>,
    ) -> connectrpc::ServiceResult<w::WorkSession> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.start_work(&request)).await
    }
    #[tracing::instrument(name = "rpc", skip_all, fields(method = "send_work", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
    async fn send_work(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::SendWorkRequest>,
    ) -> connectrpc::ServiceResult<w::WorkAcceptance> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.send_work(&request)).await
    }
    #[tracing::instrument(name = "rpc", skip_all, fields(method = "stop_work", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
    async fn stop_work(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::StopWorkRequest>,
    ) -> connectrpc::ServiceResult<ene_rpc::Empty> {
        let id = request.work_id.to_owned();
        self.call(move |kernel| {
            kernel.stop_work(&id)?;
            Ok(ene_rpc::Empty::default())
        })
        .await
    }
    #[tracing::instrument(name = "rpc", skip_all, fields(method = "close_work", companion_id = tracing::field::Empty, request_id = tracing::field::Empty))]
    async fn close_work(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::WorkRequest>,
    ) -> connectrpc::ServiceResult<ene_rpc::Empty> {
        let id = request.work_id.to_owned();
        self.call(move |kernel| {
            kernel.close_work(&id)?;
            Ok(ene_rpc::Empty::default())
        })
        .await
    }
}
