//! Neutral, exposure-calibrated lighting for the desktop avatar.

use bevy::{camera::Exposure, prelude::*};

pub const EXPOSURE: Exposure = Exposure::BLENDER;

pub fn setup(app: &mut App) {
    // Keep a fully lit diffuse surface at its authored color: 80% key + 20% fill.
    // Directional illuminance is divided by PI by the diffuse BRDF; Bevy's
    // ambient brightness already represents the integrated diffuse response.
    app.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 0.2 / EXPOSURE.exposure(),
        ..default()
    })
    .add_systems(Startup, |mut commands: Commands| {
        commands.spawn((
            DirectionalLight {
                color: Color::WHITE,
                illuminance: 0.8 * std::f32::consts::PI / EXPOSURE.exposure(),
                ..default()
            },
            Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.4, -0.3, 0.)),
        ));
    });
}
