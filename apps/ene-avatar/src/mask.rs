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

pub fn alpha_runs(data: &[u8], width: u32, height: u32, scale: u32) -> Vec<Run> {
    if scale == 0 || data.len() != width as usize * height as usize * 4 {
        return Vec::new();
    }
    let mut runs = Vec::new();
    for y in 0..height.div_ceil(scale) {
        let mut start = None;
        for x in 0..=width.div_ceil(scale) {
            let opaque = x * scale < width
                && (y * scale..((y + 1) * scale).min(height)).any(|py| {
                    (x * scale..((x + 1) * scale).min(width))
                        .any(|px| data[((py * width + px) * 4 + 3) as usize] > 8)
                });
            match (start, opaque) {
                (None, true) => start = Some(x),
                (Some(s), false) => {
                    runs.push(Run {
                        x: s as i32,
                        y: y as i32,
                        width: (x - s) as i32,
                    });
                    start = None;
                }
                _ => {}
            }
        }
    }
    runs
}

/// Verification only: compare coverage in logical coordinates, counting both
/// transparent pixels intercepted and visible pixels missed by a candidate.
pub fn coverage_errors(
    reference: &[Run],
    candidate: &[Run],
    width: u32,
    height: u32,
) -> (u64, u64) {
    fn coverage(runs: &[Run], width: u32, height: u32) -> Vec<bool> {
        let mut pixels = vec![false; width as usize * height as usize];
        for run in runs {
            if run.y < 0 || run.y >= height as i32 {
                continue;
            }
            let start = run.x.clamp(0, width as i32) as usize;
            let end = (run.x.saturating_add(run.width)).clamp(0, width as i32) as usize;
            if start < end {
                pixels[run.y as usize * width as usize + start
                    ..run.y as usize * width as usize + end]
                    .fill(true);
            }
        }
        pixels
    }
    let expected = coverage(reference, width, height);
    let actual = coverage(candidate, width, height);
    expected
        .iter()
        .zip(actual)
        .fold((0, 0), |(fp, fn_), (&e, a)| {
            (fp + u64::from(a && !e), fn_ + u64::from(e && !a))
        })
}
