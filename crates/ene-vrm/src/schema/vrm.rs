use super::{expressions::Expressions, look_at::Gaze, version::Version};
use bevy::{math::Affine3A, platform::collections::HashMap, prelude::*};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, Deserialize, Reflect, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AvatarPermission {
    #[default]
    OnlyAuthor,
    OnlySeparatelyLicensedPerson,
    Everyone,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Reflect, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CommercialUsage {
    #[default]
    PersonalNonProfit,
    PersonalProfit,
    Corporation,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Reflect, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CreditNotation {
    #[default]
    Required,
    Unnecessary,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Reflect, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Modification {
    #[default]
    Prohibited,
    AllowModification,
    AllowModificationRedistribution,
}

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
    pub license_url: String,
    pub avatar_permission: AvatarPermission,
    pub allow_excessively_violent_usage: bool,
    pub allow_excessively_sexual_usage: bool,
    pub commercial_usage: CommercialUsage,
    pub allow_political_or_religious_usage: bool,
    pub allow_antisocial_or_hate_usage: bool,
    pub credit_notation: CreditNotation,
    pub allow_redistribution: bool,
    pub modification: Modification,
    pub other_license_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Model {
    #[serde(rename = "specVersion")]
    pub _version: Version,
    pub meta: VrmMeta,
    pub humanoid: Humanoid,
    #[serde(default)]
    pub expressions: Expressions,
    pub look_at: Option<Gaze>,
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
