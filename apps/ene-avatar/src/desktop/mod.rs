//! Desktop output placement, cameras, hit regions, and pointer interaction.

mod camera;
mod hit_regions;
mod output;
mod placement;
mod pointer;

#[cfg(test)]
mod tests;

pub use hit_regions::HitRegions;
pub use output::{Output, pick_output, resolved_position, window};
#[cfg(windows)]
pub(crate) use output::{bounds, desktop_position};
pub use pointer::Drag;
pub(crate) use pointer::{PointerInput, pointer};

use crate::character::CharacterId;
use bevy::{camera::visibility::VisibilitySystems, prelude::*};

pub fn setup(app: &mut App) {
    // 1 FPS needs a full second plus scheduling jitter before the stall cap.
    app.insert_resource(Time::<Virtual>::from_max_delta(
        std::time::Duration::from_secs(2),
    ));
    app.init_resource::<Drag>()
        .add_message::<PointerInput>()
        .add_systems(PreUpdate, pointer)
        .add_systems(Update, (camera::cameras, placement::place_avatars))
        .add_systems(Update, sync_virtual_time)
        .add_observer(|_: On<Remove<Output>>, mut drag: ResMut<Drag>| drag.entity = None)
        .add_observer(|event: On<Remove<CharacterId>>, mut drag: ResMut<Drag>| {
            if drag.entity == Some(event.entity) {
                drag.entity = None;
            }
        })
        .add_systems(
            PostUpdate,
            hit_regions::regions
                .after(VisibilitySystems::CalculateBounds)
                .after(VisibilitySystems::VisibilityPropagate)
                .before(VisibilitySystems::CheckVisibility),
        );
}

/// No output surfaces means no simulation; the Wayland runner only translates
/// compositor events.
fn sync_virtual_time(outputs: Query<&Output>, mut time: ResMut<Time<Virtual>>) {
    if outputs.is_empty() {
        time.pause();
    } else {
        time.unpause();
    }
}
