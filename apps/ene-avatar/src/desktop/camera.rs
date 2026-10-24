//! One camera per desktop output.

use super::output::Output;
use bevy::{
    camera::{RenderTarget, ScalingMode},
    prelude::*,
    window::WindowRef,
};

pub(super) fn cameras(mut commands: Commands, outputs: Query<(Entity, &Output), Changed<Output>>) {
    for (entity, output) in &outputs {
        let center = output.origin + output.size * 0.5;
        commands.entity(entity).insert((
            Camera3d::default(),
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::Fixed {
                    width: output.size.x,
                    height: output.size.y,
                },
                near: 0.0,
                far: 100_000.0,
                ..OrthographicProjection::default_3d()
            }),
            RenderTarget::Window(WindowRef::Entity(entity)),
            Transform::from_xyz(center.x, -center.y, 50_000.0),
            Msaa::Off,
            bevy::core_pipeline::tonemapping::Tonemapping::None,
        ));
    }
}
