//! Desktop services and capability reporting.
use super::character::{CharacterAction, CharacterTarget};
use crate::WindowCapabilities;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DesktopOperation {
    ClipboardRead,
    ClipboardWrite {
        text: String,
    },
    Notify {
        title: String,
        body: String,
    },
    SelectFile,
    CaptureScreen,
    Avatar {
        target: CharacterTarget,
        action: CharacterAction,
    },
    ReadResource {
        handle: String,
        offset: u64,
        length: u32,
    },
    CloseResource {
        handle: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceHandle {
    pub id: String,
    pub media_type: String,
    pub size: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopCapabilities {
    pub device_id: String,
    pub generation: u64,
    pub services: Vec<String>,
    #[serde(default)]
    pub windows: WindowCapabilities,
}
