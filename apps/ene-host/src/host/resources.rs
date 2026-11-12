//! Host-wide Bevy resources, split by concern instead of one god object.
use super::model::ModelSource;
use bevy_ecs::prelude::*;
use ene_rpc as w;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Resource, Default)]
pub(crate) struct DeviceStreams(pub(crate) HashMap<String, mpsc::Sender<w::DeviceOperation>>);

#[derive(Resource, Default)]
pub(crate) struct HostState {
    pub(crate) failed: Option<String>,
    pub(crate) closing: bool,
}

impl HostState {
    pub(crate) fn ready(&self) -> super::error::Result<()> {
        if let Some(reason) = &self.failed {
            return Err(super::error::HostError::unavailable(reason.clone()));
        }
        if self.closing {
            return Err(super::error::HostError::unavailable(
                "host is shutting down",
            ));
        }
        Ok(())
    }
}

#[derive(Resource, Clone)]
pub(crate) struct Models(pub(crate) Arc<dyn ModelSource>);

#[derive(Resource)]
pub(crate) struct DeviceTimeout(pub(crate) Duration);

#[derive(Resource, Default)]
pub(crate) struct ConsolidationQueue(pub(crate) HashSet<String>);

#[derive(Resource, Default)]
pub(crate) struct CompactionQueue(pub(crate) HashSet<String>);

#[derive(Resource, Default)]
pub(crate) struct Inactivity(pub(crate) HashMap<String, Duration>);

#[derive(Resource, Clone)]
pub(crate) struct HostOptions {
    pub(crate) consolidate: bool,
    pub(crate) compaction: super::compaction::Options,
}

#[derive(Resource, Default)]
pub(crate) struct Perception(pub(crate) std::collections::HashMap<String, String>);

#[derive(Resource, Default)]
pub(crate) struct ApprovalHolds(
    pub(crate) std::collections::HashMap<String, bevy_ecs::prelude::Entity>,
);
