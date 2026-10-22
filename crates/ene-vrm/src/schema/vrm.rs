use super::{expressions::Expressions, look_at::Gaze, version::Version};
use bevy::{math::Affine3A, platform::collections::HashMap, prelude::*};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, Reflect)]
#[serde(default, rename_all = "camelCase")]
pub struct VrmMeta {
    pub name: String,
    pub version: Option<String>,
    pub authors: Vec<String>,
    pub copyright_information: Option<String>,
    pub contact_information: Option<String>,
    pub references: Vec<String>,
    pub third_party_licenses: Option<String>,
    pub thumbnail_image: Option<usize>,
    pub license_url: Option<String>,
    pub avatar_permission: Option<String>,
    pub allow_excessively_violent_usage: Option<bool>,
    pub allow_excessively_sexual_usage: Option<bool>,
    pub commercial_usage: Option<String>,
    pub allow_political_or_religious_usage: Option<bool>,
    pub allow_antisocial_or_hate_usage: Option<bool>,
    pub credit_notation: Option<String>,
    pub allow_redistribution: Option<bool>,
    pub modification: Option<String>,
    pub other_license_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Model {
    #[serde(rename = "specVersion")]
    pub _version: Version,
    #[serde(default)]
    pub meta: Option<VrmMeta>,
    pub humanoid: Humanoid,
    #[serde(default)]
    pub first_person: FirstPerson,
    #[serde(default)]
    pub expressions: Expressions,
    pub look_at: Option<Gaze>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct FirstPerson {
    pub mesh_annotations: Vec<MeshAnnotation>,
}

#[derive(Deserialize)]
pub(crate) struct MeshAnnotation {
    pub node: usize,
    #[serde(default, rename = "type")]
    pub _type: FirstPersonType,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum FirstPersonType {
    #[default]
    Auto,
    Both,
    ThirdPersonOnly,
    FirstPersonOnly,
}

#[derive(Deserialize)]
pub(crate) struct Node {
    pub node: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Humanoid {
    pub human_bones: HashMap<String, Node>,
}

impl Humanoid {
    pub fn hips_height(
        &self,
        world: &World,
        nodes: &HashMap<usize, Entity>,
        root_inv: Affine3A,
    ) -> f32 {
        self.human_bones
            .get("hips")
            .and_then(|n| world.get::<GlobalTransform>(*nodes.get(&n.node)?))
            .map(|g| root_inv.transform_point3(g.translation()).y.abs())
            .filter(|&h| h >= f32::EPSILON)
            .unwrap_or(1.)
    }
}

pub(crate) fn humanoid_parent(name: &str) -> Option<&'static str> {
    Some(match name {
        "hips" => "",
        "spine" | "leftUpperLeg" | "rightUpperLeg" => "hips",
        "chest" => "spine",
        "upperChest" => "chest",
        "neck" | "leftShoulder" | "rightShoulder" => "upperChest",
        "head" => "neck",
        "leftEye" | "rightEye" | "jaw" => "head",
        "leftLowerLeg" => "leftUpperLeg",
        "leftFoot" => "leftLowerLeg",
        "leftToes" => "leftFoot",
        "rightLowerLeg" => "rightUpperLeg",
        "rightFoot" => "rightLowerLeg",
        "rightToes" => "rightFoot",
        "leftUpperArm" => "leftShoulder",
        "leftLowerArm" => "leftUpperArm",
        "leftHand" => "leftLowerArm",
        "rightUpperArm" => "rightShoulder",
        "rightLowerArm" => "rightUpperArm",
        "rightHand" => "rightLowerArm",
        "leftThumbMetacarpal"
        | "leftIndexProximal"
        | "leftMiddleProximal"
        | "leftRingProximal"
        | "leftLittleProximal" => "leftHand",
        "leftThumbProximal" => "leftThumbMetacarpal",
        "leftThumbDistal" => "leftThumbProximal",
        "leftIndexIntermediate" => "leftIndexProximal",
        "leftIndexDistal" => "leftIndexIntermediate",
        "leftMiddleIntermediate" => "leftMiddleProximal",
        "leftMiddleDistal" => "leftMiddleIntermediate",
        "leftRingIntermediate" => "leftRingProximal",
        "leftRingDistal" => "leftRingIntermediate",
        "leftLittleIntermediate" => "leftLittleProximal",
        "leftLittleDistal" => "leftLittleIntermediate",
        "rightThumbMetacarpal"
        | "rightIndexProximal"
        | "rightMiddleProximal"
        | "rightRingProximal"
        | "rightLittleProximal" => "rightHand",
        "rightThumbProximal" => "rightThumbMetacarpal",
        "rightThumbDistal" => "rightThumbProximal",
        "rightIndexIntermediate" => "rightIndexProximal",
        "rightIndexDistal" => "rightIndexIntermediate",
        "rightMiddleIntermediate" => "rightMiddleProximal",
        "rightMiddleDistal" => "rightMiddleIntermediate",
        "rightRingIntermediate" => "rightRingProximal",
        "rightRingDistal" => "rightRingIntermediate",
        "rightLittleIntermediate" => "rightLittleProximal",
        "rightLittleDistal" => "rightLittleIntermediate",
        _ => return None,
    })
}
