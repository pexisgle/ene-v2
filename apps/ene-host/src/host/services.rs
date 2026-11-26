//! Non-ECS services owned by the host and shared with its agent kernel.

use super::{
    db::Store,
    mcp::Mcp,
    model::Credentials,
    notify::Notifier,
    providers::Providers,
    resources::{DeviceHub, HostState},
    voice::{Voice, VoiceEngineContext, voicevox::VoicevoxManager},
};
use parking_lot::RwLock;
use rig_core::catalog::Catalog;
use std::{path::PathBuf, sync::Arc};

pub(crate) struct HostServices {
    pub(crate) state_dir: PathBuf,
    pub(crate) store: Store,
    pub(crate) credentials: Credentials,
    pub(crate) catalog: Arc<RwLock<Catalog>>,
    pub(crate) hub: DeviceHub,
    pub(crate) providers: Providers,
    pub(crate) voice: Voice,
    #[cfg(test)]
    pub(crate) engine_overrides: parking_lot::RwLock<super::voice::Engines>,
    pub(crate) voicevox: VoicevoxManager,
    pub(crate) notifier: Notifier,
    pub(crate) health: HostState,
    pub(crate) mcp: Mcp,
}

impl HostServices {
    pub(crate) fn voice_context(&self) -> VoiceEngineContext<'_> {
        VoiceEngineContext {
            store: Some(&self.store),
            credentials: Some(&self.credentials),
            providers: Some(&self.providers),
            #[cfg(test)]
            engines: self.engine_overrides.read().clone(),
            #[cfg(not(test))]
            engines: Default::default(),
        }
    }
}
