use crate::application::HostApplication;
use std::sync::Arc;
#[derive(Clone)]
pub struct App {
    pub(crate) devices: crate::devices::Devices,
    pub(crate) audio: crate::audio::Audio,
    pub(crate) sessions: super::auth::Sessions,
    pub application: HostApplication,
    pub(crate) token: Arc<str>,
    pub(crate) web_origin: Option<Arc<str>>,
}
impl App {
    pub fn with_device_store(mut self, store: crate::infrastructure::sqlite::Store) -> Self {
        self.devices = crate::devices::Devices::new(Some(store));
        self
    }
    pub fn new(application: HostApplication, token: String) -> Self {
        Self {
            devices: crate::devices::Devices::new(None),
            audio: crate::audio::Audio::default(),
            sessions: Default::default(),
            application,
            token: token.into(),
            web_origin: None,
        }
    }
}

impl App {
    pub async fn snapshot_document(
        &self,
        subscription: Option<&str>,
    ) -> Result<ene_rpc::Snapshot, super::error::ApiError> {
        let view = self.application.snapshot(subscription).await?;
        let stored = view.stored;
        let message_cursors = view
            .message_cursors
            .into_iter()
            .map(|(id, value)| {
                (
                    id,
                    ene_rpc::HistoryCursor {
                        value,
                        ..Default::default()
                    },
                )
            })
            .collect();
        let snapshot = ene_rpc::Snapshot {
            host_id: stored.host_id,
            environment_id: stored.environment_id,
            capabilities: ene_rpc::JsonDocument { json: serde_json::json!({"limits":{"max_companions":1,"max_concurrent_tasks":1},"features":["history","notifications","chat","connection_settings","credentials","data_transfer_grants","connection_check","provider_plugins"],"adapters":self.application.providers().iter().filter(|p|p.state=="ready").map(|p|p.id.clone()).collect::<Vec<_>>(),"model_reference_formats":["provider_model_id"],"transport":{"rpc":"/rpc","audio":"/audio","audio_available":self.audio.available(),"device_operations":true}}).to_string(), ..Default::default() }.into(),
            cursor: view.cursor.into(),
            companions: stored.companions,
            messages: view.messages,
            message_cursors,
            settings: stored.settings.into(),
            grants: view.grants.into_iter().map(Into::into).collect(),
            health: view.health.into(),
            ..Default::default()
        };
        Ok(snapshot)
    }
}
