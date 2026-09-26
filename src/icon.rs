//! Menu bar icon: a drive split into two halves.
//!
//! * Left half: white = nothing to do, orange = a volume can be re-mounted,
//!   green = an operation is in progress.
//! * Right half: blue while at least one volume is mounted read-write by
//!   Remounty, white otherwise.
//!
//! The icon is rendered procedurally (anti-aliased signed distance fields), so
//! no image assets are needed. A thin dark outline keeps the white parts
//! visible on a light menu bar.

pub const WIDTH: u32 = 44;
pub const HEIGHT: u32 = 36;

const SUPERSAMPLE: u32 = 4;
const OUTLINE: f32 = 1.6;
const GAP: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Available,
    Working,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconState {
    pub activity: Activity,
    pub mounted_by_us: bool,
}

type Rgb = (f32, f32, f32);

const WHITE: Rgb = (1.0, 1.0, 1.0);
const ORANGE: Rgb = (1.0, 0.624, 0.039);
const GREEN: Rgb = (0.188, 0.820, 0.345);
const BLUE: Rgb = (0.039, 0.518, 1.0);
const OUTLINE_RGB: Rgb = (0.0, 0.0, 0.0);
const OUTLINE_ALPHA: f32 = 0.45;

impl IconState {
    fn colors(self) -> (Rgb, Rgb) {
        let left = match self.activity {
            Activity::Idle => WHITE,
            Activity::Available => ORANGE,
            Activity::Working => GREEN,
        };
        let right = if self.mounted_by_us { BLUE } else { WHITE };
        (left, right)
    }
}

/// Signed distance to a rounded rectangle centred at (cx, cy).
fn rounded_rect(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - (hw - r);
    let qy = (y - cy).abs() - (hh - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// Straight (non-premultiplied) RGBA pixels, `WIDTH * HEIGHT * 4` bytes.
pub fn render(state: IconState) -> Vec<u8> {
    let (left, right) = state.colors();
    let (w, h) = (WIDTH as f32, HEIGHT as f32);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (hw, hh, radius) = (w / 2.0 - 2.0, 8.5, 4.5);

    let mut out = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    let samples = (SUPERSAMPLE * SUPERSAMPLE) as f32;
    for py in 0..HEIGHT {
        for px in 0..WIDTH {
            // Accumulate premultiplied colour over the sub-samples.
            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let x = px as f32 + (sx as f32 + 0.5) / SUPERSAMPLE as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SUPERSAMPLE as f32;
                    let body = rounded_rect(x, y, cx, cy, hw, hh, radius);
                    let (fill, half) = if x < cx {
                        (left, body.max(x - (cx - GAP / 2.0)))
                    } else {
                        (right, body.max((cx + GAP / 2.0) - x))
                    };
                    if half > 0.0 {
                        continue;
                    }
                    let (color, alpha) = if half > -OUTLINE {
                        (OUTLINE_RGB, OUTLINE_ALPHA)
                    } else {
                        (fill, 1.0)
                    };
                    r += color.0 * alpha;
                    g += color.1 * alpha;
                    b += color.2 * alpha;
                    a += alpha;
                }
            }
            let alpha = a / samples;
            let unpremultiply = |c: f32| if a > 0.0 { c / a } else { 0.0 };
            out.push(to_byte(unpremultiply(r)));
            out.push(to_byte(unpremultiply(g)));
            out.push(to_byte(unpremultiply(b)));
            out.push(to_byte(alpha));
        }
    }
    out
}

fn to_byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(buf: &[u8], x: u32, y: u32) -> Option<[u8; 4]> {
        let i = ((y * WIDTH + x) * 4) as usize;
        let s = buf.get(i..i + 4)?;
        Some([*s.first()?, *s.get(1)?, *s.get(2)?, *s.get(3)?])
    }

    #[test]
    fn has_expected_size() {
        let buf = render(IconState {
            activity: Activity::Idle,
            mounted_by_us: false,
        });
        assert_eq!(buf.len(), (WIDTH * HEIGHT * 4) as usize);
    }

    #[test]
    fn halves_have_state_colors() {
        let buf = render(IconState {
            activity: Activity::Available,
            mounted_by_us: true,
        });
        let left = pixel(&buf, WIDTH / 4, HEIGHT / 2);
        let right = pixel(&buf, WIDTH * 3 / 4, HEIGHT / 2);
        assert_eq!(left, Some([255, 159, 10, 255]));
        assert_eq!(right, Some([10, 132, 255, 255]));
        // Corners and the gap are transparent.
        assert_eq!(pixel(&buf, 0, 0).map(|p| p[3]), Some(0));
        assert_eq!(pixel(&buf, WIDTH / 2, HEIGHT / 2).map(|p| p[3]), Some(0));
    }

    #[test]
    fn idle_is_white() {
        let buf = render(IconState {
            activity: Activity::Idle,
            mounted_by_us: false,
        });
        assert_eq!(pixel(&buf, WIDTH / 4, HEIGHT / 2), Some([255, 255, 255, 255]));
        assert_eq!(pixel(&buf, WIDTH * 3 / 4, HEIGHT / 2), Some([255, 255, 255, 255]));
        let working = render(IconState {
            activity: Activity::Working,
            mounted_by_us: false,
        });
        assert_eq!(pixel(&working, WIDTH / 4, HEIGHT / 2), Some([48, 209, 88, 255]));
    }
}

#[cfg(test)]
mod dump {
    use super::*;

    /// `REMOUNTY_ICON_DUMP=/dir cargo test dump_icons -- --ignored` writes raw RGBA files.
    #[test]
    #[ignore]
    fn dump_icons() {
        let Some(dir) = std::env::var_os("REMOUNTY_ICON_DUMP") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        for (name, activity) in [
            ("idle", Activity::Idle),
            ("available", Activity::Available),
            ("working", Activity::Working),
        ] {
            for mounted in [false, true] {
                let buf = render(IconState {
                    activity,
                    mounted_by_us: mounted,
                });
                let _ = std::fs::write(dir.join(format!("{name}-{mounted}.rgba")), buf);
            }
        }
    }
}
