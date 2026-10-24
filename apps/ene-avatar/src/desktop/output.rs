//! Desktop outputs, placement helpers, and window configuration.

use super::hit_regions::HitRegions;
use crate::character::Config;
use bevy::{
    prelude::*,
    window::{CompositeAlphaMode, PresentMode, Window, WindowResolution},
};

#[derive(Component, Clone, PartialEq)]
#[require(HitRegions, Transform, Visibility)]
pub struct Output {
    pub name: String,
    pub origin: Vec2,
    pub size: Vec2,
    pub units_per_logical_pixel: f32,
}

pub fn bounds(output: &Output) -> Rect {
    Rect::from_corners(output.origin, output.origin + output.size)
}

pub fn desktop_position(output: &Output, local: Vec2) -> Vec2 {
    output.origin + local * output.units_per_logical_pixel
}

/// The monitor a placement resolves to: the named one, or the first output.
pub fn pick_output<'a>(
    placement: &Config,
    mut outputs: impl Iterator<Item = &'a Output>,
) -> Option<&'a Output> {
    let first = outputs.next()?;
    std::iter::once(first)
        .chain(outputs)
        .find(|output| placement.monitor.as_deref() == Some(output.name.as_str()))
        .or(Some(first))
}

/// The requested logical position, or the default on the selected output.
pub fn resolved_position(placement: &Config, output: Option<&Output>) -> Option<Vec2> {
    placement.position.map(Vec2::from_array).or_else(|| {
        output.map(|output| output.size / output.units_per_logical_pixel * Vec2::new(0.5, 0.8))
    })
}

pub fn window(output: &Output) -> Window {
    let mut resolution = WindowResolution::new(output.size.x as u32, output.size.y as u32);
    resolution.set_scale_factor(output.units_per_logical_pixel);
    Window {
        title: "ene-avatar".into(),
        transparent: true,
        resolution,
        composite_alpha_mode: CompositeAlphaMode::PreMultiplied,
        present_mode: PresentMode::AutoNoVsync,
        ..default()
    }
}
