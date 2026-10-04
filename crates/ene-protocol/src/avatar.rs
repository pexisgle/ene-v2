use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AvatarSettings {
    pub model: String,
    pub motion: Option<String>,
    pub motion_playing: bool,
    pub motion_loop: bool,
    pub visible: bool,
    pub fps: u32,
    /// Avatar height relative to its default share of the output height.
    pub size_percent: u32,
    /// Signed logical margins from the bottom-right of the selected output.
    pub right: i32,
    pub bottom: i32,
    pub output: Option<String>,
}
impl Default for AvatarSettings {
    fn default() -> Self {
        Self {
            model: String::new(),
            motion: None,
            motion_playing: true,
            motion_loop: true,
            visible: true,
            fps: 30,
            size_percent: 100,
            right: 24,
            bottom: 24,
            output: None,
        }
    }
}
impl AvatarSettings {
    pub fn validate(&self) -> Result<(), String> {
        if ![30, 60].contains(&self.fps) {
            return Err("描画速度は30または60fpsを選んでください。".into());
        }
        if !(50..=200).contains(&self.size_percent) {
            return Err("キャラクターの大きさは50〜200%を選んでください。".into());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AvatarRequest {
    Configure { settings: AvatarSettings },
    Recenter,
    Shutdown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AvatarEvent {
    MotionState {
        state: MotionState,
    },
    MotionError {
        message: String,
    },
    Ready {
        model: String,
    },
    PositionChanged {
        right: i32,
        bottom: i32,
        output: Option<String>,
    },
    ShowChat,
    VisibilityChanged {
        visible: bool,
    },
    Error {
        message: String,
    },
    Stopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionState {
    None,
    Loading,
    Playing,
    Paused,
    Finished,
}
pub const MAX_IPC_LINE: usize = 64 * 1024;
