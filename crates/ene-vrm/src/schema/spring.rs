use super::version::Version;
use bevy::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Springs {
    #[serde(rename = "specVersion")]
    pub _version: Version,
    #[serde(default)]
    pub colliders: Vec<Collider>,
    #[serde(default)]
    pub collider_groups: Vec<ColliderGroup>,
    #[serde(default)]
    pub springs: Vec<Spring>,
}

#[derive(Deserialize)]
pub(crate) struct ColliderGroup {
    pub colliders: Vec<usize>,
}

#[derive(Deserialize)]
pub(crate) struct Collider {
    pub node: usize,
    pub shape: Shape,
}

#[derive(Deserialize)]
#[serde(try_from = "ShapeObject")]
pub(crate) enum Shape {
    Sphere {
        offset: Vec3,
        radius: f32,
    },
    Capsule {
        offset: Vec3,
        radius: f32,
        tail: Vec3,
    },
}

/// Accepts the glTF `extras`/`extensions` fields alongside the shape kind.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShapeObject {
    sphere: Option<Sphere>,
    capsule: Option<Capsule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sphere {
    #[serde(default)]
    offset: Vec3,
    #[serde(default)]
    radius: f32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capsule {
    #[serde(default)]
    offset: Vec3,
    #[serde(default)]
    radius: f32,
    #[serde(default)]
    tail: Vec3,
}

impl TryFrom<ShapeObject> for Shape {
    type Error = &'static str;

    fn try_from(value: ShapeObject) -> Result<Self, Self::Error> {
        match (value.sphere, value.capsule) {
            (Some(sphere), None) => Ok(Self::Sphere {
                offset: sphere.offset,
                radius: sphere.radius,
            }),
            (None, Some(capsule)) => Ok(Self::Capsule {
                offset: capsule.offset,
                radius: capsule.radius,
                tail: capsule.tail,
            }),
            _ => Err("VRMC collider requires exactly one of sphere or capsule"),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Spring {
    pub joints: Vec<Joint>,
    #[serde(default)]
    pub collider_groups: Vec<usize>,
    pub center: Option<usize>,
}

#[derive(Deserialize)]
pub(crate) struct Joint {
    pub node: usize,
    #[serde(flatten)]
    pub props: crate::SpringJointProps,
}
