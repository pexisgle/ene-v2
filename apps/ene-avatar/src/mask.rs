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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn alpha_holes_and_dpi_are_preserved() {
        let mut data = vec![0; 8 * 4 * 4];
        for (x, y) in [(0, 0), (1, 0), (4, 0), (7, 3)] {
            data[(y * 8 + x) * 4 + 3] = 255;
        }
        assert_eq!(
            alpha_runs(&data, 8, 4, 1),
            vec![
                Run {
                    x: 0,
                    y: 0,
                    width: 2
                },
                Run {
                    x: 4,
                    y: 0,
                    width: 1
                },
                Run {
                    x: 7,
                    y: 3,
                    width: 1
                }
            ]
        );
        assert_eq!(
            alpha_runs(&data, 8, 4, 2),
            vec![
                Run {
                    x: 0,
                    y: 0,
                    width: 1
                },
                Run {
                    x: 2,
                    y: 0,
                    width: 1
                },
                Run {
                    x: 3,
                    y: 1,
                    width: 1
                }
            ]
        );
        assert!(alpha_runs(&data[..3], 8, 4, 1).is_empty());
        let logical = alpha_runs(&data, 8, 4, 2);
        assert!(contains(&logical, (0.25, 0.75)));
        assert!(!contains(&logical, (1.25, 0.75)), "transparent hole");
        assert!(!contains(&logical, (-0.25, 0.75)));
        assert!(!contains(&logical, (0.25, 1.0)), "exclusive lower edge");
    }
}
