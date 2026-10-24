//! Only VRMC extension schemas; core glTF data belongs to Bevy.

mod animation;
mod constraint;
mod expressions;
mod look_at;
mod mtoon;
mod spring;
mod version;
mod vrm;

pub(crate) use animation::*;
pub(crate) use constraint::*;
pub(crate) use expressions::*;
pub(crate) use look_at::*;
pub(crate) use mtoon::*;
pub(crate) use spring::*;
#[cfg(test)]
pub(crate) use version::Version;
pub(crate) use vrm::*;
pub use vrm::{AvatarPermission, CommercialUsage, CreditNotation, Modification, VrmMeta};
