use bevy::prelude::*;
use serde::Deserialize;

fn default_offset_from_head_bone() -> Vec3 {
    Vec3::new(0., 0.06, 0.)
}

#[derive(Clone, Debug, Deserialize, Reflect)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Gaze {
    #[serde(default = "default_offset_from_head_bone")]
    pub offset_from_head_bone: Vec3,
    pub r#type: GazeType,
    pub range_map_horizontal_inner: Option<RangeMap>,
    pub range_map_horizontal_outer: Option<RangeMap>,
    pub range_map_vertical_down: Option<RangeMap>,
    pub range_map_vertical_up: Option<RangeMap>,
}

impl Default for Gaze {
    fn default() -> Self {
        Self {
            offset_from_head_bone: default_offset_from_head_bone(),
            r#type: GazeType::Bone,
            range_map_horizontal_inner: None,
            range_map_horizontal_outer: None,
            range_map_vertical_down: None,
            range_map_vertical_up: None,
        }
    }
}

impl Gaze {
    pub fn horizontal_inner(&self) -> RangeMap {
        self.resolve(self.range_map_horizontal_inner, self.r#type)
    }
    pub fn horizontal_outer(&self) -> RangeMap {
        self.resolve(self.range_map_horizontal_outer, self.r#type)
    }
    pub fn vertical_down(&self) -> RangeMap {
        self.resolve(self.range_map_vertical_down, self.r#type)
    }
    pub fn vertical_up(&self) -> RangeMap {
        self.resolve(self.range_map_vertical_up, self.r#type)
    }
    fn resolve(&self, map: Option<RangeMap>, gaze_type: GazeType) -> RangeMap {
        map.map(|map| map.resolve(gaze_type))
            .unwrap_or_else(|| RangeMap::default_for(gaze_type))
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Reflect)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GazeType {
    #[default]
    Bone,
    Expression,
}

#[derive(Clone, Copy, Debug, Deserialize, Reflect)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct RangeMap {
    pub input_max_value: f32,
    pub output_scale: Option<f32>,
}

impl Default for RangeMap {
    fn default() -> Self {
        Self {
            input_max_value: 90.,
            output_scale: None,
        }
    }
}

impl RangeMap {
    pub fn default_for(gaze_type: GazeType) -> Self {
        Self {
            input_max_value: 90.,
            output_scale: Some(if matches!(gaze_type, GazeType::Bone) {
                10.
            } else {
                1.
            }),
        }
    }
    fn resolve(self, gaze_type: GazeType) -> Self {
        Self {
            output_scale: self
                .output_scale
                .or_else(|| Self::default_for(gaze_type).output_scale),
            ..self
        }
    }
    pub fn map(self, angle: f32) -> f32 {
        (angle.abs() / self.input_max_value.max(f32::EPSILON)).min(1.)
            * self.output_scale.unwrap_or(0.)
    }
}
