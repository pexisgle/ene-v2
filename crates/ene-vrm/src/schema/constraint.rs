use super::version::Version;
use bevy::prelude::*;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeConstraint {
    #[serde(rename = "specVersion")]
    pub _version: Version,
    pub constraint: Constraint,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Constraint {
    Rotation(Rotation),
    Roll(Roll),
    Aim(Aim),
}

#[derive(Clone, Deserialize)]
pub(crate) struct Rotation {
    pub source: usize,
    pub weight: Option<f32>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Roll {
    pub source: usize,
    pub roll_axis: RollAxis,
    pub weight: Option<f32>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Aim {
    pub source: usize,
    pub aim_axis: AimAxis,
    pub weight: Option<f32>,
}

#[derive(Clone, Copy, Deserialize)]
pub(crate) enum RollAxis {
    X,
    Y,
    Z,
}

impl RollAxis {
    pub fn vector(self) -> Vec3 {
        match self {
            Self::X => Vec3::X,
            Self::Y => Vec3::Y,
            Self::Z => Vec3::Z,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
pub(crate) enum AimAxis {
    PositiveX,
    NegativeX,
    PositiveY,
    NegativeY,
    PositiveZ,
    NegativeZ,
}

impl AimAxis {
    pub fn vector(self) -> Vec3 {
        match self {
            Self::PositiveX => Vec3::X,
            Self::NegativeX => Vec3::NEG_X,
            Self::PositiveY => Vec3::Y,
            Self::NegativeY => Vec3::NEG_Y,
            Self::PositiveZ => Vec3::Z,
            Self::NegativeZ => Vec3::NEG_Z,
        }
    }
}
