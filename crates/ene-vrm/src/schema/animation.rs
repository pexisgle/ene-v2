use super::{
    version::Version,
    vrm::{Humanoid, Node},
};
use bevy::{platform::collections::HashMap, prelude::*};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Motion {
    #[serde(rename = "specVersion")]
    pub _version: Version,
    pub humanoid: Option<Humanoid>,
    #[serde(default)]
    pub expressions: MotionExpressions,
    pub look_at: Option<MotionLookAt>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct MotionLookAt {
    pub node: Option<usize>,
    pub offset_from_head_bone: Vec3,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(crate) struct MotionExpressions {
    pub preset: HashMap<String, Node>,
    pub custom: HashMap<String, Node>,
}

impl MotionExpressions {
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Node)> {
        self.preset.iter().chain(&self.custom)
    }
}
