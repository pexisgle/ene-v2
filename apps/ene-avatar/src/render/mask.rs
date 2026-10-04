//! Lossless horizontal runs of rendered alpha, in logical Wayland coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub x: i32,
    pub y: i32,
    pub width: i32,
}
pub fn contains(runs: &[Run], point: (f64, f64)) -> bool {
    runs.iter().any(|run| {
        point.0 >= run.x as f64
            && point.0 < (run.x + run.width) as f64
            && point.1 >= run.y as f64
            && point.1 < (run.y + 1) as f64
    })
}
