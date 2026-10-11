//! Own the presented lease and commit image/input coverage together.
use super::{
    GpuError,
    frames::{FrameIdentity, LeasedFrame},
    surface::GpuSurface,
};
use std::sync::Arc;

struct Target {
    width: u32,
    height: u32,
    scale: u32,
    generation: u64,
}
impl FrameIdentity {
    fn matches(self, target: &Target, last: u64) -> bool {
        self.sequence > last
            && self.width == target.width
            && self.height == target.height
            && self.scale == target.scale
            && self.generation == target.generation
    }
}
pub struct AvatarPresenter {
    surface: GpuSurface,
    current: Option<LeasedFrame>,
    sequence: u64,
}
impl AvatarPresenter {
    pub fn new(
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        window: Arc<crate::wayland::NativeWindow>,
    ) -> Result<Self, GpuError> {
        Ok(Self {
            surface: GpuSurface::new(instance, adapter, device, queue, window)?,
            current: None,
            sequence: 0,
        })
    }
    /// None means no frame was committed. Coverage changes only after successful acquisition.
    pub fn present_latest(
        &mut self,
        frames: Vec<LeasedFrame>,
        state: &mut crate::wayland::State,
        generation: u64,
    ) -> Result<Option<bool>, GpuError> {
        let target = Target {
            width: state
                .width
                .checked_mul(state.scale)
                .ok_or_else(|| GpuError("avatar extent overflow".into()))?,
            height: state
                .height
                .checked_mul(state.scale)
                .ok_or_else(|| GpuError("avatar extent overflow".into()))?,
            scale: state.scale,
            generation,
        };
        let Some(frame) = frames
            .into_iter()
            .filter(|frame| frame.identity.matches(&target, self.sequence))
            .max_by_key(|frame| frame.identity.sequence)
        else {
            return Ok(None);
        };
        let Some(output) =
            self.surface
                .draw(&frame.texture, frame.identity.width, frame.identity.height)?
        else {
            return Ok(None);
        };
        state.prepare_gpu_present(&frame.runs);
        output.present();
        self.surface.commit()?;
        let covered = !frame.runs.is_empty();
        self.sequence = frame.identity.sequence;
        self.current = Some(frame);
        Ok(Some(covered))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_require_matching_generation_extent_scale_and_new_sequence() {
        let target = Target {
            width: 800,
            height: 600,
            scale: 2,
            generation: 4,
        };
        let frame = FrameIdentity {
            width: 800,
            height: 600,
            scale: 2,
            generation: 4,
            sequence: 9,
        };
        assert!(frame.matches(&target, 8));
        assert!(!frame.matches(&target, 9));
        for stale in [
            FrameIdentity {
                generation: 3,
                ..frame
            },
            FrameIdentity { scale: 1, ..frame },
            FrameIdentity {
                width: 400,
                ..frame
            },
            FrameIdentity {
                height: 300,
                ..frame
            },
        ] {
            assert!(!stale.matches(&target, 8));
        }
    }
}
