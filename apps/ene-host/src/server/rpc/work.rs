//! Work-session services.
use super::Services;
use crate::host::work;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use ene_rpc as w;

#[expect(refining_impl_trait, reason = "Connect boxes futures")]
impl w::WorkService for Services {
    async fn list_works(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ListWorksRequest>,
    ) -> ServiceResult<w::WorkViews> {
        tracing::Span::current().record("companion_id", request.companion_id);
        let companion = request.companion_id.to_owned();
        self.with_store(move |store| work::list(&store, &companion))
            .await
    }

    async fn get_work(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::WorkRequest>,
    ) -> ServiceResult<w::WorkView> {
        tracing::Span::current().record("work_id", request.work_id);
        let id = request.work_id.to_owned();
        self.with_store(move |store| work::get(&store, &id)).await
    }

    async fn start_work(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::StartWorkRequest>,
    ) -> connectrpc::ServiceResult<w::WorkSession> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.start_work(&request)).await
    }
    async fn send_work(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::SendWorkRequest>,
    ) -> connectrpc::ServiceResult<w::WorkAcceptance> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.send_work(&request)).await
    }
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
