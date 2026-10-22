use bevy::{platform::collections::HashMap, prelude::*};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, Reflect)]
#[serde(default)]
pub(crate) struct Expressions {
    pub preset: HashMap<String, Expression>,
    pub custom: HashMap<String, Expression>,
}

impl Expressions {
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Expression)> {
        self.preset.iter().chain(&self.custom)
    }
    pub fn materials(&self) -> impl Iterator<Item = usize> {
        self.iter().flat_map(|(_, e)| {
            e.material_color_binds
                .iter()
                .map(|b| b.material)
                .chain(e.texture_transform_binds.iter().map(|b| b.material))
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Reflect)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Expression {
    pub is_binary: bool,
    pub override_blink: Override,
    pub override_mouth: Override,
    pub override_look_at: Override,
    pub morph_target_binds: Vec<MorphBind>,
    pub material_color_binds: Vec<ColorBind>,
    pub texture_transform_binds: Vec<TextureBind>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Reflect)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Override {
    #[default]
    None,
    Block,
    Blend,
}

impl Override {
    pub fn rate(self, weight: f32) -> f32 {
        match self {
            Self::None => 0.,
            Self::Block => f32::from(weight > 0.),
            Self::Blend => weight,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Reflect)]
pub(crate) struct MorphBind {
    pub node: usize,
    pub index: usize,
    pub weight: f32,
}

#[derive(Clone, Debug, Deserialize, Reflect)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ColorBind {
    pub material: usize,
    pub r#type: ColorType,
    pub target_value: Vec4,
}

#[derive(Clone, Copy, Debug, Deserialize, Reflect)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ColorType {
    Color,
    EmissionColor,
    ShadeColor,
    MatcapColor,
    RimColor,
    OutlineColor,
}

#[derive(Clone, Debug, Deserialize, Reflect)]
#[serde(default)]
pub(crate) struct TextureBind {
    pub material: usize,
    pub scale: Vec2,
    pub offset: Vec2,
}

impl Default for TextureBind {
    fn default() -> Self {
        Self {
            material: 0,
            scale: Vec2::ONE,
            offset: Vec2::ZERO,
        }
    }
}
