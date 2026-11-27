//! Operations the host serializes with its ECS schedules.

use crate::host::{
    HostServices, chat, client,
    delegation::{self, DeliveredClaims},
    error::{HostError, Result},
    eyes, mind, permission, work,
};
use ene_domain::ClientId;
use ene_rpc as w;
use rig_ecs::prelude::{App, Aside, Author, Conversation, DeliveryMode, RequestId};

/// The RPC boundary for operations that touch live agents or their tools.
pub(crate) struct AgentKernel<'a> {
    app: &'a mut App,
    services: &'a HostServices,
}

impl<'a> AgentKernel<'a> {
    pub(crate) fn new(app: &'a mut App, services: &'a HostServices) -> Self {
        Self { app, services }
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
        client::declare(self.app.world_mut(), client::Declaration::Register(request))
    }

    pub(crate) fn set_capabilities(
        &mut self,
        request: &w::SetCapabilitiesRequest,
    ) -> Result<w::Client> {
        client::declare(self.app.world_mut(), client::Declaration::Replace(request))
    }

    pub(crate) fn update_policy_rules(
        &mut self,
        additions: &[permission::PolicyRule],
        removals: &[String],
    ) -> Result<w::PolicyRules> {
        use client::dynamic::{PublicationChange, sync_tools};

        let services = self.services;
        let old = client::clients(&services.store, &services.hub)?;
        let rules = services
            .store
            .policy_repo()
            .apply_policy_rules(additions, removals)?;
        let world = self.app.world_mut();
        if let Err(error) = sync_tools(world, PublicationChange::Refresh) {
            crate::host::fail(world, &error.to_string());
            return Err(error);
        }
        for current in client::clients(&services.store, &services.hub)? {
            let previous = old.iter().find(|entry| entry.id == current.id);
            if previous
                .is_some_and(|entry| entry.granted_capabilities == current.granted_capabilities)
            {
                continue;
            }
            if !current
                .granted_capabilities
                .iter()
                .any(|name| name == w::COMPUTER_USE)
            {
                client::computer::invalidate_client(world, &current.id, None);
            }
            client::notify_client(&services.notifier, &current);
        }
        let items: Vec<_> = rules.iter().map(Into::into).collect();
        services.notifier.push(
            None,
            None,
            w::notification::Event::PolicyRulesUpdated(Box::new(w::PolicyRulesUpdated {
                rules: items.clone(),
                ..Default::default()
            })),
        );
        Ok(w::PolicyRules {
            items,
            ..Default::default()
        })
    }

    pub(crate) fn activate_client(
        &mut self,
        request: &w::ActivationRequest,
    ) -> Result<w::Activation> {
        client::activate(self.app.world_mut(), request)
    }

    pub(crate) fn client_reconnected(&mut self, client: &ClientId) -> Result<()> {
        let client_id = client.as_str();
        let services = self.services;
        let world = self.app.world_mut();
        client::computer::invalidate_client(world, client_id, services.hub.subscription(client_id));
        for id in services
            .store
            .client_repo()
            .mark_unanswered_operations_uncertain(Some(client_id))?
        {
            client::device::uncertain(
                world,
                &id,
                "the device stream was replaced; the result is unknown",
            );
        }
        let context = client::device::DeviceDispatchContext {
            store: &services.store,
            hub: &services.hub,
            notifier: &services.notifier,
        };
        client::device::resend(&context, client_id, |operation, failure| {
            if let Some(failure) = failure {
                let waiter = world
                    .resource_mut::<crate::host::resources::Waiting>()
                    .by_operation
                    .remove(operation);
                if let Some(waiter) = waiter {
                    permission::refuse(world, waiter.call, &failure.message);
                }
            } else {
                client::device::uncertain(
                    world,
                    operation,
                    "Computer Use operations are never replayed",
                );
            }
        })
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
        client: &ClientId,
    ) -> Result<bool> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(false);
        }
        let companions = self
            .services
            .store
            .client_repo()
            .active_companions_for_client(client.as_str())?;
        if companions.is_empty() || !self.services.voice.accept_transcription(id) {
            return Ok(false);
        }
        let world = self.app.world_mut();
        delegation::record(&mut world.resource_mut::<DeliveredClaims>(), id);
        let author = Author {
            request: Some(RequestId(id.to_owned())),
            ..Author::user()
        };
        for companion in companions {
            let Some(agent) = super::agent_of(world, &companion) else {
                continue;
            };
            if let Some(mut conversation) = world.get_mut::<Conversation>(agent) {
                conversation.add_aside(Aside::new(
                    "ene.transcription",
                    Author::plugin("ene-host"),
                    serde_json::json!({"transcription_id": id, "client_id": client.as_str()}),
                ));
            }
            super::deliver(
                world,
                agent,
                &companion,
                text.to_owned(),
                DeliveryMode::Steer,
                author.clone(),
            );
        }
        Ok(true)
    }

    pub(crate) fn submit_approval(
        &mut self,
        id: &str,
        decision: permission::ApprovalDecision,
    ) -> Result<w::SubmitApprovalResponse> {
        let outcome = permission::submit_with_outcome(
            self.app.world_mut(),
            permission::ApprovalRequest::Operator(id),
            decision,
        )?;
        let (rule_saved, rule_error) = match outcome.rule {
            permission::RuleSave::Saved => (true, None),
            permission::RuleSave::NotRequested => (false, None),
            permission::RuleSave::Failed(error) => (false, Some(error)),
        };
        Ok(w::SubmitApprovalResponse {
            acknowledged: true,
            status: w::ApprovalStatus::from(outcome.approval.status).into(),
            rule_saved,
            rule_error,
            ..Default::default()
        })
    }
}
