use bevy::prelude::*;
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, Reflect)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Gaze {
    pub offset_from_head_bone: Vec3,
    pub r#type: GazeType,
    pub range_map_horizontal_inner: Option<RangeMap>,
    pub range_map_horizontal_outer: Option<RangeMap>,
    pub range_map_vertical_down: Option<RangeMap>,
    pub range_map_vertical_up: Option<RangeMap>,
}

impl Gaze {
    pub fn horizontal_inner(&self) -> RangeMap {
        self.range_map_horizontal_inner
            .unwrap_or_else(|| RangeMap::default_for(self.r#type))
    }
    pub fn horizontal_outer(&self) -> RangeMap {
        self.range_map_horizontal_outer
            .unwrap_or_else(|| RangeMap::default_for(self.r#type))
    }
    pub fn vertical_down(&self) -> RangeMap {
        self.range_map_vertical_down
            .unwrap_or_else(|| RangeMap::default_for(self.r#type))
    }
    pub fn vertical_up(&self) -> RangeMap {
        self.range_map_vertical_up
            .unwrap_or_else(|| RangeMap::default_for(self.r#type))
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
    pub output_scale: f32,
}

impl Default for RangeMap {
    fn default() -> Self {
        Self {
            input_max_value: 90.,
            output_scale: 10.,
        }
    }
}

impl RangeMap {
    pub fn default_for(gaze_type: GazeType) -> Self {
        Self {
            input_max_value: 90.,
            output_scale: if matches!(gaze_type, GazeType::Bone) {
                10.
            } else {
                1.
            },
        }
    }
    pub fn map(self, angle: f32) -> f32 {
        (angle.abs() / self.input_max_value.max(f32::EPSILON)).min(1.) * self.output_scale
    }
}
