//! VRM 1.0 extensions for Bevy's native glTF, scene, material and animation APIs.
#![forbid(unsafe_code)]
#![recursion_limit = "256"]

mod animation;
mod expressions;
mod loader;
mod material;
mod runtime;
mod schema;

use bevy::{app::AnimationSystems, prelude::*, transform::TransformSystems};
pub use expressions::ExpressionWeights;
pub use runtime::{GazeOverride, SpringJointProps, SpringRig, Vrm};
pub use vrm_spec::vrmc_vrm_1_0::{HumanBoneName, Meta};

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VrmSystems {
    Pose,
    LookAt,
    Expressions,
    Constraints,
    SpringBone,
}

#[derive(Debug)]
pub struct VrmPlugin;

impl Plugin for VrmPlugin {
    fn build(&self, app: &mut App) {
        loader::setup(app);
        material::setup(app);
        app.configure_sets(
            PostUpdate,
            (
                VrmSystems::Pose,
                VrmSystems::LookAt,
                VrmSystems::Expressions,
                VrmSystems::Constraints,
                VrmSystems::SpringBone,
            )
                .chain()
                .after(AnimationSystems)
                .before(TransformSystems::Propagate),
        );
        animation::setup(app);
        expressions::setup(app);
        runtime::setup(app);
    }
    fn finish(&self, app: &mut App) {
        loader::finish(app);
    }
}

#[cfg(test)]
mod tests;
