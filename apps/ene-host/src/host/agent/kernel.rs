//! Operations the host serializes with its ECS schedules.

use crate::host::{
    HostServices, chat, client,
    error::{HostError, Result},
    eyes, mind, permission, voice, wire, work,
};
use ene_rpc as w;
use rig_core::catalog::Catalog;
use rig_ecs::prelude::App;

/// The RPC boundary for operations that touch live agents or their tools.
pub(crate) struct AgentKernel<'a> {
    app: &'a mut App,
    services: &'a HostServices,
}

impl<'a> AgentKernel<'a> {
    pub(crate) fn new(app: &'a mut App, services: &'a HostServices) -> Self {
        Self { app, services }
    }

    pub(crate) fn services(&self) -> &HostServices {
        self.services
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        self.app
            .world()
            .resource::<rig_ecs::model::Models>()
            .0
            .catalog()
    }

    pub(crate) fn accept(&mut self, request: &w::SendMessageRequest) -> Result<w::Acceptance> {
        chat::accept(self.app, request)
    }

    pub(crate) fn stop_turn(&mut self, request: &w::StopRequest) -> Result<()> {
        chat::stop(self.app.world_mut(), request)
    }

    pub(crate) fn history(&mut self, request: &w::HistoryRequestView<'_>) -> Result<w::History> {
        chat::history(self.app.world_mut(), request)
    }

    pub(crate) fn message(&mut self, request: &w::MessageRequestView<'_>) -> Result<w::Message> {
        chat::message_by_id(self.app.world_mut(), request)
    }

    pub(crate) fn create_companion(
        &mut self,
        request: w::CreateCompanionRequest,
    ) -> Result<w::Companion> {
        mind::create(self.app.world_mut(), request)
    }

    pub(crate) fn update_companion(
        &mut self,
        request: w::UpdateCompanionRequest,
    ) -> Result<w::Companion> {
        mind::update(self.app.world_mut(), request)
    }

    pub(crate) fn delete_companion(&mut self, id: &str) -> Result<()> {
        mind::delete(self.app.world_mut(), id)
    }

    pub(crate) fn import_companion(
        &mut self,
        request: w::ImportCompanionRequest,
    ) -> Result<w::Companion> {
        mind::import_template(self.app.world_mut(), request)
    }

    pub(crate) fn rebind_companion(&mut self, id: &str) -> Result<()> {
        super::sync_companion(self.app.world_mut(), id)
    }

    pub(crate) fn start_work(&mut self, request: &w::StartWorkRequest) -> Result<w::WorkSession> {
        work::start(self.app.world_mut(), request)
    }

    pub(crate) fn send_work(&mut self, request: &w::SendWorkRequest) -> Result<w::WorkAcceptance> {
        work::send(self.app.world_mut(), request)
    }

    pub(crate) fn stop_work(&mut self, id: &str) -> Result<()> {
        work::stop(self.app.world_mut(), id)
    }

    pub(crate) fn close_work(&mut self, id: &str) -> Result<()> {
        work::close(self.app.world_mut(), id)
    }

    pub(crate) fn register_client(
        &mut self,
        request: &w::RegisterClientRequest,
    ) -> Result<w::Client> {
        client::register(self.app.world_mut(), request)
    }

    pub(crate) fn set_capabilities(
        &mut self,
        request: &w::SetCapabilitiesRequest,
    ) -> Result<w::Client> {
        client::set_capabilities(self.app.world_mut(), request)
    }

    pub(crate) fn activate_client(
        &mut self,
        request: &w::ActivationRequest,
    ) -> Result<w::Activation> {
        client::activate(self.app.world_mut(), request)
    }

    pub(crate) fn client_reconnected(&mut self, client_id: &str) {
        client::device::resend(self.app.world_mut(), client_id);
    }

    pub(crate) fn client_disconnected(&mut self, client_id: &str) {
        client::device::disconnected(self.app.world_mut(), client_id);
    }

    pub(crate) fn complete_operation(
        &mut self,
        request: &w::DeviceOperationResult,
    ) -> Result<w::DeviceOperationState> {
        client::device::complete(self.app.world_mut(), request)
    }

    pub(crate) fn operation_state(&self, id: &str) -> Result<w::DeviceOperationState> {
        client::device::get_state(
            &self.services.store,
            self.app
                .world()
                .get_resource::<client::computer::ComputerUse>(),
            &self.services.hub,
            id,
        )
    }

    pub(crate) fn report_perception(&mut self, event: &w::PerceptionEvent) -> Result<()> {
        self.app
            .world_mut()
            .run_system_cached_with(client::ingest_perception, event)
            .map_err(|error| HostError::Internal(error.to_string()))?
    }

    pub(crate) fn screen_preview(&self, id: &str) -> Result<w::ScreenFrame> {
        eyes::preview(self.app.world().resource(), &self.services.hub, id)
    }

    pub(crate) fn accept_transcription(
        &mut self,
        id: &str,
        text: &str,
        binding: &voice::InputBinding,
    ) -> Result<bool> {
        voice::accept_transcription(self.app.world_mut(), id, text, binding)
    }

    pub(crate) fn submit_approval(
        &mut self,
        id: &str,
        decision: permission::ApprovalDecision,
    ) -> Result<w::SubmitApprovalResponse> {
        let outcome = permission::submit_with_outcome(self.app.world_mut(), id, decision)?;
        Ok(w::SubmitApprovalResponse {
            acknowledged: true,
            status: wire::approval_status(outcome.approval.status).into(),
            rule_saved: outcome.rule_saved,
            rule_error: outcome.rule_error,
            ..Default::default()
        })
    }
}
