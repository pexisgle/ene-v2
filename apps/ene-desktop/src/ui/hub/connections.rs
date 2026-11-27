//! Connection CRUD, model/voice selection, probing, credentials, and preset entrypoints on [`Hub`].

use super::Hub;
use crate::rpc;
use ene_client_core::{
    connections::build_connection_enabled_input,
    domain::{Connection, ConnectionInput, ConnectionKind},
};
use gpui::Context;

impl Hub {
    pub(crate) fn load_providers(&mut self, _cx: &mut Context<Self>) {
        if self.model.connections.providers.is_some() {
            return;
        }
        rpc::connections::load_providers(&self.rpc, self.model.connections.link_epoch);
    }

    pub(crate) fn load_models(&mut self, _cx: &mut Context<Self>) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        let ids: Vec<String> = settings
            .connections
            .iter()
            .filter(|connection| connection.kind == ConnectionKind::Llm)
            .map(|connection| connection.id.clone())
            .collect();
        if !ids.is_empty() {
            rpc::connections::load_models(&self.rpc, self.model.connections.link_epoch, ids);
        }
    }

    pub(crate) fn upsert_connection(
        &mut self,
        context: &'static str,
        input: ConnectionInput,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        self.update_connections(context, settings.revision, vec![input], None, cx);
    }

    pub(crate) fn delete_connection(&mut self, id: String, revision: u64, cx: &mut Context<Self>) {
        rpc::connections::delete_connection(
            &self.rpc,
            self.model.connections.link_epoch,
            id,
            revision,
        );
        cx.notify();
    }

    pub(crate) fn add_mcp_connection(
        &mut self,
        input: ConnectionInput,
        secret: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(settings) = self.model.settings() else {
            return false;
        };
        let id = input.id.clone();
        let Some(ticket) = rpc::connections::add_mcp_and_check(
            &self.rpc,
            self.model.connections.link_epoch,
            settings.revision,
            input,
            secret,
        ) else {
            return false;
        };
        self.setup.checking_connections.insert(id, ticket);
        cx.notify();
        true
    }

    pub(crate) fn set_connection_enabled(
        &mut self,
        connection: &Connection,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        let input = build_connection_enabled_input(connection, enabled);
        let probe = (enabled && connection.is_local_voicevox()).then(|| connection.id.clone());
        self.update_connections("接続状態の更新", settings.revision, vec![input], probe, cx);
    }

    pub(super) fn update_connections(
        &mut self,
        context: &'static str,
        revision: u64,
        inputs: Vec<ConnectionInput>,
        probe: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let ticket = rpc::connections::update_settings(
            &self.rpc,
            self.model.connections.link_epoch,
            context,
            revision,
            inputs,
            probe.clone(),
        );
        if let Some((id, ticket)) = probe.zip(ticket) {
            self.setup.checking_connections.insert(id, ticket);
        }
        cx.notify();
    }

    pub(crate) fn check_connection(&mut self, connection_id: String, cx: &mut Context<Self>) {
        let Some(revision) = self.model.settings().map(|settings| settings.revision) else {
            return;
        };
        self.check_connection_at(connection_id, revision, None, cx);
    }

    pub(crate) fn check_connection_at(
        &mut self,
        connection_id: String,
        revision: u64,
        resource_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.setup.checking_connections.contains_key(&connection_id) {
            return;
        }
        if let Some(ticket) = rpc::connections::probe_connection(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_id.clone(),
            revision,
            resource_id,
        ) {
            self.setup
                .checking_connections
                .insert(connection_id, ticket);
            cx.notify();
        }
    }

    pub(crate) fn register_secret(
        &mut self,
        connection_id: &str,
        secret: &str,
        revision: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim();
        if secret.is_empty() || !self.rpc.has_host() {
            return;
        }
        rpc::connections::register_secret(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_id.to_owned(),
            secret.to_owned(),
            revision,
        );
        cx.notify();
    }

    pub(crate) fn register_secret_for_connections(
        &mut self,
        connection_ids: Vec<String>,
        secret: &str,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim();
        if secret.is_empty() || connection_ids.is_empty() {
            return;
        }
        rpc::connections::register_secret_for_connections(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_ids,
            secret.to_owned(),
            revision,
        );
        cx.notify();
    }

    pub(crate) fn revoke_secret(&mut self, connection_id: String, cx: &mut Context<Self>) {
        rpc::connections::revoke_secret(
            &self.rpc,
            self.model.connections.link_epoch,
            connection_id,
        );
        cx.notify();
    }
}
