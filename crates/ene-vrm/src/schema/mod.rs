//! VRMC extension data not covered by `vrm-spec`, plus shared conversions.

mod constraint;
mod mtoon;
mod version;

pub(crate) use constraint::*;
pub(crate) use mtoon::*;
#[cfg(test)]
pub(crate) use version::Version;
pub use vrm_spec::vrmc_vrm_1_0::{ExpressionPresetName, HumanBoneName, Meta as VrmMeta};
pub(crate) use vrm_spec::{
    vrmc_spring_bone_1_0::VrmcSpringBoneSchema as Springs,
    vrmc_vrm_1_0::{
        Expression, ExpressionOverrideType, Expressions, LookAt, LookAtRangeMap, LookAtType,
        MaterialColorType, VRMCVrmSchema as Model,
    },
    vrmc_vrm_animation_1_0::VRMCVrmAnimationSchema as Motion,
};

/// Index value as authored. `vrm-spec` types node/material references as
/// `gltf::json::Index`, including `TextureTransformBind::material` which the
/// spec classifies as a material but types as a node index.
pub(crate) fn index<T>(index: gltf::json::Index<T>) -> usize {
    index.value()
}
