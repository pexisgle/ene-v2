use super::version::Version;
use bevy::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Springs {
    #[serde(rename = "specVersion")]
    pub _version: Version,
    #[serde(default)]
    pub colliders: Vec<Collider>,
    #[serde(default)]
    pub collider_groups: Vec<ColliderGroup>,
    #[serde(default)]
    pub springs: Vec<Spring>,
}

#[derive(Deserialize)]
pub(crate) struct ColliderGroup {
    pub colliders: Vec<usize>,
}

#[derive(Deserialize)]
pub(crate) struct Collider {
    pub node: usize,
    pub shape: Shape,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Shape {
    Sphere {
        #[serde(default)]
        offset: Vec3,
        #[serde(default)]
        radius: f32,
    },
    Capsule {
        #[serde(default)]
        offset: Vec3,
        #[serde(default)]
        radius: f32,
        #[serde(default)]
        tail: Vec3,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Spring {
    pub joints: Vec<Joint>,
    #[serde(default)]
    pub collider_groups: Vec<usize>,
    pub center: Option<usize>,
}

#[derive(Deserialize)]
pub(crate) struct Joint {
    pub node: usize,
    #[serde(flatten)]
    pub props: crate::SpringJointProps,
}
