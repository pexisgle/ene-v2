use bevy::prelude::*;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeConstraint {
    pub spec_version: String,
    pub constraint: Constraint,
}

/// The spec's `oneOf`: exactly one of these is set, which `compile_constraints`
/// enforces.
#[derive(Clone, Deserialize)]
pub(crate) struct Constraint {
    pub rotation: Option<Rotation>,
    pub roll: Option<Roll>,
    pub aim: Option<Aim>,
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

/// The spec spells these `"X"`, `"Y"`, `"Z"` (unlike the camelCase names
/// elsewhere in VRM).
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

/// The spec spells these `"PositiveX"` ... `"NegativeZ"`.
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
