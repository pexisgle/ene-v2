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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_constraint_reads_spec_axis_names_and_ignores_extras() {
        let roll: NodeConstraint = serde_json::from_value(json!({
            "specVersion": "1.0",
            "constraint": {
                "roll": {"source": 2, "rollAxis": "Y", "weight": 0.5},
                "extras": {"note": "authoring metadata"},
                "extensions": {}
            }
        }))
        .unwrap();
        assert_eq!(roll.spec_version, "1.0");
        let roll = roll.constraint.roll.unwrap();
        assert_eq!(roll.roll_axis.vector(), Vec3::Y);
        assert_eq!(roll.weight, Some(0.5));

        let aim: NodeConstraint = serde_json::from_value(json!({
            "specVersion": "1.0",
            "constraint": {"aim": {"source": 3, "aimAxis": "NegativeZ"}}
        }))
        .unwrap();
        let aim = aim.constraint.aim.unwrap();
        assert_eq!(aim.aim_axis.vector(), Vec3::NEG_Z);
        assert_eq!(aim.weight, None);
    }
}
