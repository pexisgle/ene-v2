//! Avatar model, asset loading, and per-frame lifecycle.

mod assets;
mod lifecycle;
mod model;

#[cfg(test)]
mod tests;

pub(crate) use assets::{ModelAssets, reload};
pub(crate) use lifecycle::notify_finished;
pub use model::{AvatarState, CharacterId, Config, Notification};

use bevy::{app::AnimationSystems, prelude::*};
use ene_vrm::VrmSystems;

pub fn setup(app: &mut App) {
    app.add_message::<Notification>()
        .add_systems(
            PostUpdate,
            (assets::finish_retries, assets::load_scene).before(lifecycle::update_avatars),
        )
        .add_systems(
            PostUpdate,
            lifecycle::update_avatars
                .before(AnimationSystems)
                .before(bevy::camera::visibility::VisibilitySystems::VisibilityPropagate),
        )
        .add_systems(
            PostUpdate,
            lifecycle::notify_finished.after(AnimationSystems),
        )
        .configure_sets(
            PostUpdate,
            VrmSystems::SpringBone
                .before(bevy::camera::visibility::VisibilitySystems::CalculateBounds),
        );
}
