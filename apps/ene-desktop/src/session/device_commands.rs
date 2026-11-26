//! Typed input and output for the client's built-in device capabilities.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub(super) struct MotionInput {
    pub motion: String,
    pub expression: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct LookAtInput {
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub reset: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct PerchInput {
    pub x: f32,
    pub y: f32,
    pub monitor: Option<String>,
    pub posture: Option<RestingPosture>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RestingPosture {
    Idle,
    SitIdle,
    SitSwingLegs,
}

impl RestingPosture {
    pub fn preset(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::SitIdle => "sit_idle",
            Self::SitSwingLegs => "sit_swing_legs",
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum OperationOutput {
    Motion {
        applied: bool,
        motion: String,
        expression: Option<String>,
    },
    LookAt {
        reset: bool,
        yaw_deg: Option<f32>,
        pitch_deg: Option<f32>,
    },
    Perch(PerchInput),
    Watch {
        applied: bool,
        enabled: bool,
    },
    /// Plugins, computer use and workspace tools have their own result schemas.
    External(serde_json::Value),
}
