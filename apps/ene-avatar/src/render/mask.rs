//! Lossless horizontal runs of rendered alpha, in logical Wayland coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub x: i32,
    pub y: i32,
    pub width: i32,
}
#[cfg(unix)]
pub fn contains(runs: &[Run], point: (f64, f64)) -> bool {
    runs.iter().any(|run| {
        point.0 >= run.x as f64
            && point.0 < (run.x + run.width) as f64
            && point.1 >= run.y as f64
            && point.1 < (run.y + 1) as f64
    })
}

#[cfg(windows)]
/// Combine only identical intervals on adjacent rows, preserving every hole.
pub fn rectangles(runs: &[Run]) -> Vec<(Run, i32)> {
    let mut rectangles: Vec<(Run, i32)> = Vec::new();
    let mut last = std::collections::BTreeMap::new();
    for &run in runs {
        let key = (run.x, run.width);
        if let Some(&index) = last.get(&key) {
            let (previous, height): &mut (Run, i32) = &mut rectangles[index];
            if previous.y + *height == run.y {
                *height += 1;
                continue;
            }
        }
        last.insert(key, rectangles.len());
        rectangles.push((run, 1));
    }
    rectangles
}
